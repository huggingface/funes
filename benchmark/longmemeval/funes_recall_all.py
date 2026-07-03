"""After indexing: run `funes recall` per question, save top-k as funes_topk.json.

Usage: python3 funes_recall_all.py <workdir> [k]
"""
import json, os, re, sys, subprocess, glob

FUNES = os.environ.get("FUNES_BIN", "/home/ubuntu/funes/target/release/funes")
WORK = sys.argv[1]
K = int(sys.argv[2]) if len(sys.argv) > 2 else 10
GET = re.compile(r"(?:->|→)\s*get\s+(\S+)\s+(\S+)")

env = dict(os.environ)
env["FASTEMBED_CACHE_DIR"] = "/home/ubuntu/funes/.fastembed_cache"

for qdir in sorted(glob.glob(os.path.join(WORK, "*"))):
    if not os.path.isdir(qdir):
        continue
    gold = json.load(open(os.path.join(qdir, "gold.json")))
    e = dict(env)
    e["FUNES_HOME"] = os.path.join(qdir, "store")
    r = subprocess.run([FUNES, "recall", gold["question"], "--k", str(K), "--half-life", "0"],
                       env=e, capture_output=True, text=True)
    top = []
    for sid, uuid in GET.findall(r.stdout):
        idx = int(uuid.split("::t")[-1]) if "::t" in uuid else None
        top.append({"session_id": sid, "turn_idx": idx})
    json.dump(top, open(os.path.join(qdir, "funes_topk.json"), "w"))
    print(f"{os.path.basename(qdir)}: hits={len(top)} rc={r.returncode}")
print("FUNES RECALL DONE")
