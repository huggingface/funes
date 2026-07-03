"""Naive dense search over one question's haystack (query embedded at call time).
Backend for the dense_search pi tool. NAIVE_QID env selects the question.
Usage: lme_venv/bin/python naive_search.py "<query>" [k]
"""
import os, sys
import numpy as np
from fastembed import TextEmbedding

SP = os.environ.get("LME_WORK", ".")
qid = os.environ["NAIVE_QID"]
query = sys.argv[1]
k = int(sys.argv[2]) if len(sys.argv) > 2 else 8

WORKROOT = os.environ.get("NAIVE_WORKROOT", f"{SP}/work50")
d = np.load(f"{WORKROOT}/{qid}/naive_emb.npz", allow_pickle=True)
model = TextEmbedding(model_name="BAAI/bge-small-en-v1.5")
q = np.array(list(model.embed([query]))[0], dtype=np.float32)
q /= (np.linalg.norm(q) + 1e-9)
sims = d["emb"] @ q
order = np.argsort(-sims)[:k]

for i in order:
    print(f"[{str(d['ts'][i])[:10]}] {d['role'][i]} (session {d['sid'][i]}): {d['text'][i]}")
    print("---")
