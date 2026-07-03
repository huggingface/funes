"""End-to-end reader+judge runner for the funes-vs-baselines LongMemEval experiment.

Arms (reader = local GLM-4.5-Air, all short-context, k=10 retrieved turns):
  no_memory : question only
  naive     : BGE-small dense top-k        (naive_topk.json)
  funes     : funes recall top-k           (funes_topk.json)
  oracle    : gold evidence sessions only  (ceiling; skipped if > token budget)

Judge = GLM-5.2 via the HF router. Relative A/B: same reader+judge+prompt across arms.

Modes:
  read   : build contexts, call the reader, write <qdir>/answers.json  (resumable)
  judge  : call the judge on each answer,   write <qdir>/verdicts.json (resumable)
  report : aggregate accuracy by arm and question type

Usage: python3 lme_run.py <read|judge|report> <workdir> [--force] [--only qid]
"""
import json, os, re, sys, glob, time
import requests
from concurrent.futures import ThreadPoolExecutor

READER_URL = "http://127.0.0.1:8001/v1/chat/completions"
READER_MODELS = "http://127.0.0.1:8001/v1/models"
JUDGE_URL = "https://router.huggingface.co/v1/chat/completions"
JUDGE_MODEL = "zai-org/GLM-5.2"
K = 10
ORACLE_TOKEN_BUDGET = 15000       # ~16384 minus answer+scaffold reserve
ARMS = ["no_memory", "naive", "funes", "oracle"]

SYS_CTX = ("You answer questions about the user based on excerpts from their past "
           "conversations, each shown with its date. Use only the provided excerpts. "
           "If the excerpts do not contain the answer, reply exactly: I don't know. "
           "Answer concisely.")
SYS_NOMEM = ("You answer questions about the user. If you do not have the information "
             "to answer, reply exactly: I don't know. Answer concisely.")

_tok = open(os.path.expanduser("~/.cache/huggingface/token")).read().strip()
_reader_model = None


# ---------- data ----------
def turn_map(qdir):
    """(session_id, turn_idx) -> {role, content, ts}, plus session order by ts."""
    m = {}
    for f in sorted(glob.glob(os.path.join(qdir, "projects", "lme", "*.jsonl"))):
        sid = os.path.splitext(os.path.basename(f))[0]
        for line in open(f):
            r = json.loads(line)
            idx = int(r["uuid"].split("::t")[-1])
            m[(sid, idx)] = {"role": r["message"]["role"],
                             "content": r["message"]["content"], "ts": r["timestamp"]}
    return m


def fmt_turns(turns):
    # chronological (ts, session, idx) so knowledge-update / temporal read naturally
    turns = sorted(turns, key=lambda t: (t["ts"], t.get("sid", ""), t.get("idx", 0)))
    lines = [f"[{t['ts'][:10]}] {t['role']}: {t['content']}" for t in turns]
    return "\n".join(lines)


def ctx_from_topk(qdir, tmap, topk):
    turns = []
    for h in topk:
        key = (h["session_id"], h["turn_idx"])
        if key in tmap:
            t = dict(tmap[key]); t["sid"], t["idx"] = key
            turns.append(t)
    return fmt_turns(turns)


def ctx_oracle(qdir, tmap, gold):
    gs = set(gold["gold_session_ids"])
    turns = []
    for (sid, idx), t in tmap.items():
        if sid in gs:
            t = dict(t); t["sid"], t["idx"] = sid, idx
            turns.append(t)
    return fmt_turns(turns)


def est_tokens(s):
    return len(s) // 4


# ---------- reader ----------
def reader_model():
    global _reader_model
    if _reader_model is None:
        d = requests.get(READER_MODELS, timeout=30).json()
        _reader_model = d["data"][0]["id"]
    return _reader_model


def reader_call(system, user):
    body = {"model": reader_model(),
            "messages": [{"role": "system", "content": system},
                         {"role": "user", "content": user}],
            "temperature": 0, "max_tokens": 256}
    r = requests.post(READER_URL, json=body, timeout=600)
    r.raise_for_status()
    return r.json()["choices"][0]["message"]["content"].strip()


def build(qdir, gold, tmap, arm):
    if arm == "no_memory":
        return SYS_NOMEM, f"Question: {gold['question']}\nAnswer:", 0
    if arm == "naive":
        ctx = ctx_from_topk(qdir, tmap, json.load(open(f"{qdir}/naive_topk.json")))
    elif arm == "funes":
        ctx = ctx_from_topk(qdir, tmap, json.load(open(f"{qdir}/funes_topk.json")))
    elif arm == "oracle":
        ctx = ctx_oracle(qdir, tmap, gold)
    user = f"Past conversation excerpts:\n{ctx}\n\nQuestion: {gold['question']}\nAnswer:"
    return SYS_CTX, user, est_tokens(ctx)


def do_read(qdir, force):
    gold = json.load(open(f"{qdir}/gold.json"))
    out_path = f"{qdir}/answers.json"
    out = json.load(open(out_path)) if os.path.exists(out_path) and not force else {}
    tmap = turn_map(qdir)
    need = {"naive": "naive_topk.json", "funes": "funes_topk.json"}
    for arm in ARMS:
        if arm in out:
            continue
        req = need.get(arm)
        if req and not os.path.exists(os.path.join(qdir, req)):
            continue  # retrieval not ready yet; a later run fills this arm
        system, user, ntok = build(qdir, gold, tmap, arm)
        if arm == "oracle" and ntok > ORACLE_TOKEN_BUDGET:
            out[arm] = {"answer": None, "skipped": "oversize", "ctx_tokens": ntok}
            continue
        t0 = time.time()
        ans = reader_call(system, user)
        out[arm] = {"answer": ans, "ctx_tokens": ntok, "sec": round(time.time() - t0, 1)}
        json.dump(out, open(out_path, "w"), indent=2)  # checkpoint each arm
    return gold["question_id"], {a: out[a].get("answer") for a in out}


# ---------- judge ----------
def judge_call(gold, ans):
    if ans is None:
        return None
    if gold["is_abstention"]:
        prompt = ("This question cannot be answered from the user's history, so the correct "
                  "behavior is to abstain. The model answer is CORRECT if it declines, says it "
                  "doesn't know, or says the information isn't available; INCORRECT if it "
                  "fabricates a specific answer. Reply with exactly 'yes' or 'no'.\n\n"
                  f"Question: {gold['question']}\nModel answer: {ans}\n\nDid it correctly abstain?")
    else:
        prompt = ("Grade the model's answer against the reference. It is correct if it conveys "
                  "the same key information as the reference (paraphrase or superset is fine). "
                  "Reply with exactly 'yes' or 'no'.\n\n"
                  f"Question: {gold['question']}\nReference answer: {gold['answer']}\n"
                  f"Model answer: {ans}\n\nIs the model answer correct?")
    body = {"model": JUDGE_MODEL,
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0, "max_tokens": 16,
            "chat_template_kwargs": {"enable_thinking": False}}
    for attempt in range(4):
        try:
            r = requests.post(JUDGE_URL, headers={"Authorization": f"Bearer {_tok}"},
                              json=body, timeout=120)
            r.raise_for_status()
            txt = r.json()["choices"][0]["message"]["content"].strip().lower()
            return {"correct": txt.startswith("yes"), "raw": txt}
        except Exception as e:
            if attempt == 3:
                return {"correct": None, "error": str(e)[:200]}
            time.sleep(2 * (attempt + 1))


def do_judge(qdir, force):
    gold = json.load(open(f"{qdir}/gold.json"))
    answers = json.load(open(f"{qdir}/answers.json"))
    vp = f"{qdir}/verdicts.json"
    verd = json.load(open(vp)) if os.path.exists(vp) and not force else {}
    for arm, a in answers.items():
        if arm in verd:
            continue
        verd[arm] = judge_call(gold, a.get("answer"))
        json.dump(verd, open(vp, "w"), indent=2)
    return qdir


# ---------- report ----------
def do_report(work):
    from collections import defaultdict
    tot = defaultdict(lambda: [0, 0])          # arm -> [correct, total]
    by_type = defaultdict(lambda: defaultdict(lambda: [0, 0]))
    oracle_skips = 0
    for qdir in sorted(glob.glob(f"{work}/*")):
        if not os.path.isdir(qdir):
            continue
        gold = json.load(open(f"{qdir}/gold.json"))
        vp = f"{qdir}/verdicts.json"
        if not os.path.exists(vp):
            continue
        verd = json.load(open(vp))
        qt = gold["question_type"] + ("_abs" if gold["is_abstention"] else "")
        for arm, v in verd.items():
            if v is None or v.get("correct") is None:
                if arm == "oracle":
                    oracle_skips += 1
                continue
            c = 1 if v["correct"] else 0
            tot[arm][0] += c; tot[arm][1] += 1
            by_type[qt][arm][0] += c; by_type[qt][arm][1] += 1

    def pct(cell):
        return f"{100*cell[0]/cell[1]:4.0f}% ({cell[0]}/{cell[1]})" if cell[1] else "   -   "

    print("\n==== ACCURACY BY ARM ====")
    for arm in ARMS:
        print(f"  {arm:10s} {pct(tot[arm])}")
    print(f"  (oracle skipped as oversize: {oracle_skips})")
    print("\n==== BY QUESTION TYPE ====")
    print(f"  {'type':30s} " + " ".join(f"{a:>14s}" for a in ARMS))
    for qt in sorted(by_type):
        print(f"  {qt:30s} " + " ".join(f"{pct(by_type[qt][a]):>14s}" for a in ARMS))
    json.dump({"total": dict(tot), "by_type": {k: dict(v) for k, v in by_type.items()}},
              open(f"{work}/results.json", "w"), indent=2)
    print(f"\nsaved {work}/results.json")


# ---------- main ----------
def main():
    mode, work = sys.argv[1], sys.argv[2]
    force = "--force" in sys.argv
    only = None
    if "--only" in sys.argv:
        only = sys.argv[sys.argv.index("--only") + 1]
    qdirs = [d for d in sorted(glob.glob(f"{work}/*")) if os.path.isdir(d)]
    if only:
        qdirs = [d for d in qdirs if os.path.basename(d) == only]

    if mode == "read":
        print("reader model:", reader_model())
        for d in qdirs:
            qid, ans = do_read(d, force)
            print(f"{qid}: " + " | ".join(f"{a}={str(ans[a])[:40]!r}" for a in ans))
    elif mode == "judge":
        with ThreadPoolExecutor(max_workers=8) as ex:
            list(ex.map(lambda d: do_judge(d, force), qdirs))
        print(f"judged {len(qdirs)} questions")
    elif mode == "report":
        do_report(work)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
