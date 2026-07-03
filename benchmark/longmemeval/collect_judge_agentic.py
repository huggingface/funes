"""Collect agentic answers + tool-use, judge with GLM-5.2, and report.
Usage: python3 collect_judge_agentic.py <collect|report>
"""
import json, os, glob, sys, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lme_run  # reuse judge_call

SP = os.environ.get("LME_WORK", ".")
WROOT = os.environ.get("WROOT", f"{SP}/work50")
AROOT = os.environ.get("AROOT", f"{SP}/agentic")
ARMS = ["naive", "funes"]


def tool_counts(sess_dir):
    c = collections.Counter()
    for f in glob.glob(f"{sess_dir}/*.jsonl"):
        for line in open(f):
            try:
                r = json.loads(line)
            except Exception:
                continue
            if r.get("type") != "message":
                continue
            content = r.get("message", {}).get("content")
            if isinstance(content, list):
                for part in content:
                    if isinstance(part, dict) and part.get("type") == "toolCall":
                        c[part.get("name", "?")] += 1
    return dict(c)


def collect():
    for qdir in sorted(glob.glob(f"{WROOT}/*")):
        if not os.path.isdir(qdir):
            continue
        qid = os.path.basename(qdir)
        gold = json.load(open(f"{qdir}/gold.json"))
        out = {}
        for arm in ARMS:
            adir = f"{AROOT}/{arm}/{qid}"
            ans_p = f"{adir}/answer.txt"
            if not os.path.exists(ans_p):
                out[arm] = {"answer": None, "tools": {}, "verdict": None}
                continue
            ans = open(ans_p).read().strip()
            tools = tool_counts(f"{adir}/sessions")
            verdict = lme_run.judge_call(gold, ans if ans else None)
            out[arm] = {"answer": ans, "tools": tools, "verdict": verdict}
        json.dump(out, open(f"{qdir}/agentic.json", "w"), indent=2)
        print(f"{qid}: " + " | ".join(
            f"{a}={out[a]['verdict'] and out[a]['verdict'].get('correct')}"
            f"/tools={out[a]['tools']}" for a in ARMS))


def report():
    tot = {a: [0, 0] for a in ARMS}
    inter = {a: [0, 0] for a in ARMS}          # both arms answered (fair head-to-head)
    by_type = collections.defaultdict(lambda: {a: [0, 0] for a in ARMS})
    behav = {a: {"called_tool": 0, "calls": 0, "used_get": 0, "n": 0} for a in ARMS}
    missing = {a: [] for a in ARMS}
    for qdir in sorted(glob.glob(f"{WROOT}/*")):
        if not os.path.isdir(qdir):
            continue
        ap = f"{qdir}/agentic.json"
        if not os.path.exists(ap):
            continue
        qid = os.path.basename(qdir)
        gold = json.load(open(f"{qdir}/gold.json"))
        qt = gold["question_type"] + ("_abs" if gold["is_abstention"] else "")
        d = json.load(open(ap))
        vok = {a: (d[a]["verdict"] and d[a]["verdict"].get("correct") is not None) for a in ARMS}
        both = all(vok[a] for a in ARMS)
        for arm in ARMS:
            v = d[arm]["verdict"]
            if vok[arm]:
                c = 1 if v["correct"] else 0
                tot[arm][0] += c; tot[arm][1] += 1
                by_type[qt][arm][0] += c; by_type[qt][arm][1] += 1
                if both:
                    inter[arm][0] += c; inter[arm][1] += 1
            else:
                missing[arm].append(qid)
            t = tool_counts(f"{AROOT}/{arm}/{qid}/sessions")
            b = behav[arm]; b["n"] += 1
            calls = sum(t.values())
            b["calls"] += calls
            b["called_tool"] += 1 if calls else 0
            b["used_get"] += 1 if t.get("get") else 0

    def pct(cell):
        return f"{100*cell[0]/cell[1]:4.0f}% ({cell[0]}/{cell[1]})" if cell[1] else "   -   "

    print("\n==== AGENTIC ACCURACY (GLM-5.2 judge, all answered) ====")
    for a in ARMS:
        print(f"  {a:8s} {pct(tot[a])}")
    print("\n==== HEAD-TO-HEAD (intersection: both arms answered) ====")
    for a in ARMS:
        print(f"  {a:8s} {pct(inter[a])}")
    for a in ARMS:
        if missing[a]:
            print(f"  [{a} produced NO answer on {len(missing[a])}: {missing[a]}]")
    print("\n==== BY QUESTION TYPE ====")
    print(f"  {'type':30s} {'naive':>14s} {'funes':>14s}")
    for qt in sorted(by_type):
        print(f"  {qt:30s} {pct(by_type[qt]['naive']):>14s} {pct(by_type[qt]['funes']):>14s}")
    print("\n==== TOOL-USE BEHAVIOR ====")
    for a in ARMS:
        b = behav[a]; n = b["n"] or 1
        print(f"  {a:8s} called_tool {b['called_tool']}/{b['n']}  "
              f"mean_calls {b['calls']/n:.2f}  used_get {b['used_get']}/{b['n']}")
    json.dump({"total": tot, "by_type": {k: dict(v) for k, v in by_type.items()},
               "behavior": behav}, open(f"{AROOT}/agentic_results.json", "w"), indent=2)
    print(f"\nsaved {AROOT}/agentic_results.json")


if __name__ == "__main__":
    (collect if sys.argv[1] == "collect" else report)()
