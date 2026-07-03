"""Precompute BGE-small doc embeddings per question for the naive dense_search tool.
Writes work50/<qid>/naive_emb.npz. Usage: lme_venv/bin/python naive_precompute.py <workdir>
"""
import json, os, glob, sys
import numpy as np
from fastembed import TextEmbedding

WORK = sys.argv[1]
model = TextEmbedding(model_name="BAAI/bge-small-en-v1.5")

for qdir in sorted(glob.glob(f"{WORK}/*")):
    if not os.path.isdir(qdir):
        continue
    turns = []
    for f in sorted(glob.glob(f"{qdir}/projects/lme/*.jsonl")):
        sid = os.path.splitext(os.path.basename(f))[0]
        for line in open(f):
            r = json.loads(line)
            idx = int(r["uuid"].split("::t")[-1])
            turns.append((sid, idx, r["message"]["content"], r["timestamp"], r["message"]["role"]))
    emb = np.array(list(model.embed([t[2] for t in turns])), dtype=np.float32)
    emb /= (np.linalg.norm(emb, axis=1, keepdims=True) + 1e-9)
    np.savez(f"{qdir}/naive_emb.npz", emb=emb,
             sid=np.array([t[0] for t in turns]), idx=np.array([t[1] for t in turns]),
             text=np.array([t[2] for t in turns], dtype=object),
             ts=np.array([t[3] for t in turns]), role=np.array([t[4] for t in turns]))
    print(os.path.basename(qdir), len(turns))
print("PRECOMPUTE DONE")
