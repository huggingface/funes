"""Retrieval smoke test for the LongMemEval adapter.

Per question, `funes index` its haystack then `funes recall` the question, and
check whether the gold evidence (session id and turn uuid) surfaces in the top-k
hits. This exercises the adapter + provenance mapping without a reader model.

Usage: python3 smoke_test.py <workdir> [k]
  <workdir>  holds per-question subdirs produced by adapter.py
  [k]        results to request (default 10)

Env:
  FUNES_BIN             path to the funes binary (default: "funes" on PATH)
  FASTEMBED_CACHE_DIR   shared embed/rerank model cache (optional; avoids re-download)

Recall runs with --half-life 0: funes recency is wall-clock-relative, but
LongMemEval dates are historical, so recency weighting is meaningless here.
"""
import json, os, re, subprocess, sys, time

FUNES = os.environ.get("FUNES_BIN", "funes")
GET = re.compile(r"(?:->|→)\s*get\s+(\S+)\s+(\S+)")


def run(cmd, env):
    return subprocess.run(cmd, env=env, capture_output=True, text=True)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    workdir = sys.argv[1]
    k = int(sys.argv[2]) if len(sys.argv) > 2 else 10
    base = dict(os.environ)  # inherits FASTEMBED_CACHE_DIR if set

    summary = []
    for qid in sorted(os.listdir(workdir)):
        qdir = os.path.join(workdir, qid)
        if not os.path.isdir(qdir):
            continue
        gold = json.load(open(os.path.join(qdir, "gold.json")))
        env = dict(base)
        env["FUNES_HOME"] = os.path.join(qdir, "store")

        t0 = time.time()
        idx = run([FUNES, "index", qdir, "--harness", "claude"], env)
        t_idx = time.time() - t0
        t0 = time.time()
        rec = run([FUNES, "recall", gold["question"], "--k", str(k), "--half-life", "0"], env)
        t_rec = time.time() - t0

        hits = GET.findall(rec.stdout)
        hit_sids = [h[0] for h in hits]
        hit_turns = [h[1] for h in hits]
        gsess, gturn = set(gold["gold_session_ids"]), set(gold["gold_turn_uuids"])
        sess_hit = any(s in gsess for s in hit_sids)
        turn_hit = any(t in gturn for t in hit_turns)
        sess_rank = next((i + 1 for i, s in enumerate(hit_sids) if s in gsess), None)
        turn_rank = next((i + 1 for i, t in enumerate(hit_turns) if t in gturn), None)

        print(f"\n=== {qid}  ({gold['question_type']}) ===")
        print(f"Q: {gold['question']}   |  A: {gold['answer']}")
        print(f"{gold['n_sessions']} sessions / {gold['n_turns']} turns   "
              f"index {t_idx:.1f}s  recall {t_rec:.1f}s   hits={len(hits)}")
        print(f"gold_sessions={sorted(gsess)}  gold_turns={sorted(gturn)}")
        print(f"SESSION HIT@{k}: {sess_hit} (rank {sess_rank})   "
              f"TURN HIT@{k}: {turn_hit} (rank {turn_rank})")
        if idx.returncode != 0:
            print("INDEX rc", idx.returncode, "STDERR:", idx.stderr[-600:])
        if rec.returncode != 0:
            print("RECALL rc", rec.returncode, "STDERR:", rec.stderr[-600:])
        if not hits and rec.returncode == 0:
            print("RECALL STDOUT (head):", rec.stdout[:600])
        else:
            print("top hits (sid8 / turn):",
                  [(s[:8], t.split("::")[-1]) for s, t in hits[:6]])
        summary.append((qid, sess_hit, turn_hit, turn_rank))

    print("\n===== SUMMARY =====")
    for qid, sh, th, tr in summary:
        print(f"{qid}: session_hit={sh} turn_hit={th} turn_rank={tr}")


if __name__ == "__main__":
    main()
