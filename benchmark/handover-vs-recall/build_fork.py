#!/usr/bin/env python3
"""Build a clean pre-handoff fork of a Claude session for a handover-vs-recall experiment.

Keeps the transcript up to (and including) line CUT, rewrites sessionId to a fresh deterministic
UUID so it can't collide with the original when placed in a projects dir, and writes <out>.jsonl.
The source is never modified. Fails loudly if a /compact summary would leak into the fork.

  ./build_fork.py <source-session.jsonl> <cut-line> <out.jsonl> [label]

CUT is 1-indexed and inclusive: lines 1..CUT are kept, line CUT+1 (the first excluded turn) is the
boundary. Print the new session id and set it as FORK_ID in the experiment's config.sh.
"""
import json, hashlib, os, sys

if len(sys.argv) < 4:
    sys.exit(__doc__)
SRC, CUT, OUT = sys.argv[1], int(sys.argv[2]), sys.argv[3]
LABEL = sys.argv[4] if len(sys.argv) > 4 else "handover-fork"

raw = open(SRC, errors="replace").read().splitlines()
if CUT > len(raw):
    sys.exit(f"cut {CUT} exceeds session length {len(raw)}")
kept = raw[:CUT]

seed = f"{os.path.basename(SRC)}:{CUT}:{LABEL}".encode()
h = hashlib.sha256(seed).hexdigest()
new_id = f"{h[0:8]}-{h[8:12]}-4{h[13:16]}-8{h[17:20]}-{h[20:32]}"

rewritten, last_ts, kinds = [], None, {"user": 0, "assistant": 0}
for l in kept:
    try:
        o = json.loads(l)
    except Exception:
        rewritten.append(l)
        continue
    if "sessionId" in o:
        o["sessionId"] = new_id
    if o.get("timestamp"):
        last_ts = o["timestamp"]
    if o.get("type") in kinds:
        kinds[o["type"]] += 1
    rewritten.append(json.dumps(o))

joined = "\n".join(rewritten)
assert "isCompactSummary" not in joined, "compact summary leaked into the fork"
assert "continued from a previous conversation" not in joined, "compact text leaked into the fork"

os.makedirs(os.path.dirname(os.path.abspath(OUT)), exist_ok=True)
with open(OUT, "w") as f:
    f.write(joined + "\n")

tip = ""
for l in reversed(rewritten):
    try:
        o = json.loads(l)
    except Exception:
        continue
    if o.get("type") == "assistant":
        c = o.get("message", {}).get("content")
        if isinstance(c, list):
            c = " ".join(x.get("text", "") for x in c if isinstance(x, dict) and x.get("type") == "text")
        if str(c).strip():
            tip = str(c)[:140]
            break

print("new session id :", new_id, "  <- set FORK_ID to this")
print("cut line       :", CUT, "(inclusive; boundary is line", CUT + 1, ")")
print("wrote          :", OUT)
print("lines / turns  :", len(rewritten), f"(user={kinds['user']} assistant={kinds['assistant']})")
print("last timestamp :", last_ts)
print("tip assistant  :", tip)
