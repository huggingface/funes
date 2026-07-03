"""Naive dense-RAG baseline: BGE-small cosine top-k over each question's haystack.

Same embedding model as funes's vector leg, symmetric (no rerank, no BM25 fusion,
no recency) -- so the funes-vs-naive gap isolates funes's pipeline. Writes
work50/<qid>/naive_topk.json = [{session_id, turn_idx, score}, ...].

Usage: lme_venv/bin/python naive_rag.py <workdir> [k]
"""
import json, os, sys, glob
import numpy as np
from fastembed import TextEmbedding

WORK = sys.argv[1]
K = int(sys.argv[2]) if len(sys.argv) > 2 else 10
model = TextEmbedding(model_name="BAAI/bge-small-en-v1.5")


def load_turns(qdir):
    turns = []  # (session_id, turn_idx, text)
    for f in sorted(glob.glob(os.path.join(qdir, "projects", "lme", "*.jsonl"))):
        sid = os.path.splitext(os.path.basename(f))[0]
        for line in open(f):
            r = json.loads(line)
            idx = int(r["uuid"].split("::t")[-1])
            turns.append((sid, idx, r["message"]["content"]))
    return turns


def norm(a):
    return a / (np.linalg.norm(a, axis=-1, keepdims=True) + 1e-9)


for qdir in sorted(glob.glob(os.path.join(WORK, "*"))):
    if not os.path.isdir(qdir):
        continue
    gold = json.load(open(os.path.join(qdir, "gold.json")))
    turns = load_turns(qdir)
    doc = norm(np.array(list(model.embed([t[2] for t in turns]))))
    q = norm(np.array(list(model.embed([gold["question"]]))[0]))
    sims = doc @ q
    order = np.argsort(-sims)[:K]
    top = [{"session_id": turns[i][0], "turn_idx": turns[i][1], "score": float(sims[i])}
           for i in order]
    json.dump(top, open(os.path.join(qdir, "naive_topk.json"), "w"))
    print(f"{os.path.basename(qdir)}: top1={top[0]['session_id']}::t{top[0]['turn_idx']} "
          f"sim={top[0]['score']:.3f}")
print("NAIVE-RAG DONE")
