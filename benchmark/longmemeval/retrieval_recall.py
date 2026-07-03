"""Controlled retrieval recall@k (question as query), funes vs naive, independent of the reader.
Answers: does naive dense recall degrade at _m scale while funes holds up?
Usage: lme_venv/bin/python retrieval_recall.py <workroot> [k]
"""
import json, os, glob, re, subprocess, sys
import numpy as np
from fastembed import TextEmbedding

WROOT = sys.argv[1]
K = int(sys.argv[2]) if len(sys.argv) > 2 else 10
FUNES = os.environ.get("FUNES_BIN", "/home/ubuntu/funes/target/release/funes")
GET = re.compile(r"(?:->|→)\s*get\s+(\S+)\s+(\S+)")
env = dict(os.environ); env["FASTEMBED_CACHE_DIR"] = "/home/ubuntu/funes/.fastembed_cache"
model = TextEmbedding(model_name="BAAI/bge-small-en-v1.5")

fs = ft = ns = nt = n = 0
for qdir in sorted(glob.glob(f"{WROOT}/*")):
    if not os.path.isdir(qdir):
        continue
    g = json.load(open(f"{qdir}/gold.json"))
    if g["is_abstention"] or not g["gold_turn_uuids"]:
        continue
    n += 1
    gsess, gturn = set(g["gold_session_ids"]), set(g["gold_turn_uuids"])
    # funes recall
    e = dict(env); e["FUNES_HOME"] = f"{qdir}/store"
    r = subprocess.run([FUNES, "recall", g["question"], "--k", str(K), "--half-life", "0"],
                       env=e, capture_output=True, text=True)
    hits = GET.findall(r.stdout)
    if {h[0] for h in hits} & gsess: fs += 1
    if {h[1] for h in hits} & gturn: ft += 1
    # naive dense
    d = np.load(f"{qdir}/naive_emb.npz", allow_pickle=True)
    q = np.array(list(model.embed([g["question"]]))[0], dtype=np.float32); q /= np.linalg.norm(q) + 1e-9
    order = np.argsort(-(d["emb"] @ q))[:K]
    if {str(d["sid"][i]) for i in order} & gsess: ns += 1
    if {f"{d['sid'][i]}::t{d['idx'][i]}" for i in order} & gturn: nt += 1

print(f"{WROOT.split('/')[-1]}  n={n}  k={K}")
print(f"  funes: session-hit@{K}={fs}/{n} ({100*fs//n}%)  turn-hit@{K}={ft}/{n} ({100*ft//n}%)")
print(f"  naive: session-hit@{K}={ns}/{n} ({100*ns//n}%)  turn-hit@{K}={nt}/{n} ({100*nt//n}%)")
