"""Exact reader token consumption per arm.

Reconstructs each arm's prompt (lme_run.build), gets prompt_tokens from the server's
usage (max_tokens=1), and completion tokens by tokenizing the stored answer.

Usage: python3 measure_tokens.py [limit]
"""
import sys, json, glob, os, requests
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import lme_run

SP = os.environ.get("LME_WORK", ".")
B = "http://127.0.0.1:8001"
LIMIT = int(sys.argv[1]) if len(sys.argv) > 1 else 10**9
ARMS = lme_run.ARMS
NEED = {"naive": "naive_topk.json", "funes": "funes_topk.json"}


def prompt_tokens(system, user):
    body = {"model": lme_run.reader_model(),
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
            "temperature": 0, "max_tokens": 1}
    return requests.post(f"{B}/v1/chat/completions", json=body, timeout=600).json()["usage"]["prompt_tokens"]


def tok(text):
    if not text:
        return 0
    return len(requests.post(f"{B}/tokenize", json={"content": text}, timeout=120).json()["tokens"])


agg = {a: {"prompt": 0, "completion": 0, "n": 0, "pmax": 0} for a in ARMS}
for i, qdir in enumerate(sorted(glob.glob(f"{SP}/work50/*"))):
    if not os.path.isdir(qdir) or i >= LIMIT:
        continue
    gold = json.load(open(f"{qdir}/gold.json"))
    tmap = lme_run.turn_map(qdir)
    ans = json.load(open(f"{qdir}/answers.json"))
    for arm in ARMS:
        a = ans.get(arm, {})
        if a.get("answer") is None:
            continue
        need = NEED.get(arm)
        if need and not os.path.exists(f"{qdir}/{need}"):
            continue
        system, user, _ = lme_run.build(qdir, gold, tmap, arm)
        pt = prompt_tokens(system, user)
        ct = tok(a["answer"])
        agg[arm]["prompt"] += pt
        agg[arm]["completion"] += ct
        agg[arm]["n"] += 1
        agg[arm]["pmax"] = max(agg[arm]["pmax"], pt)

print(f"\n{'arm':10s} {'n':>3s} {'prompt_tot':>11s} {'prompt_mean':>12s} {'prompt_max':>11s} "
      f"{'compl_tot':>10s} {'compl_mean':>11s} {'total_tokens':>13s}")
for arm in ARMS:
    d = agg[arm]
    n = d["n"] or 1
    print(f"{arm:10s} {d['n']:>3d} {d['prompt']:>11d} {d['prompt']/n:>12.0f} {d['pmax']:>11d} "
          f"{d['completion']:>10d} {d['completion']/n:>11.1f} {d['prompt']+d['completion']:>13d}")
json.dump(agg, open(f"{SP}/token_usage.json", "w"), indent=2)
print(f"\nsaved {SP}/token_usage.json")
