"""GPU BGE-small precompute for the naive dense_search corpus (vllm venv: torch+cuda+transformers).
Same npz format as naive_precompute.py. BGE = CLS pooling + L2 normalize (matches fastembed).

Usage: /home/ubuntu/vllm/.venv/bin/python gpu_naive.py <workdir>
"""
import os, sys, glob, json
import numpy as np, torch
from transformers import AutoTokenizer, AutoModel

WORK = sys.argv[1]
DEV = "cuda:0"
tok = AutoTokenizer.from_pretrained("BAAI/bge-small-en-v1.5")
model = AutoModel.from_pretrained("BAAI/bge-small-en-v1.5").to(DEV).half().eval()


@torch.no_grad()
def embed(texts, bs=256):
    out = []
    for i in range(0, len(texts), bs):
        enc = tok(texts[i:i + bs], padding=True, truncation=True, max_length=512,
                  return_tensors="pt").to(DEV)
        cls = model(**enc).last_hidden_state[:, 0]
        cls = torch.nn.functional.normalize(cls, p=2, dim=1)
        out.append(cls.float().cpu().numpy())
    return np.concatenate(out) if out else np.zeros((0, 384), dtype=np.float32)


for qdir in sorted(glob.glob(f"{WORK}/*")):
    if not os.path.isdir(qdir):
        continue
    turns = []
    for f in sorted(glob.glob(os.path.join(qdir, "projects", "lme", "*.jsonl"))):
        sid = os.path.splitext(os.path.basename(f))[0]
        for line in open(f):
            r = json.loads(line)
            idx = int(r["uuid"].split("::t")[-1])
            turns.append((sid, idx, r["message"]["content"], r["timestamp"], r["message"]["role"]))
    emb = embed([t[2] for t in turns]).astype(np.float32)
    np.savez(os.path.join(qdir, "naive_emb.npz"), emb=emb,
             sid=np.array([t[0] for t in turns]), idx=np.array([t[1] for t in turns]),
             text=np.array([t[2] for t in turns], dtype=object),
             ts=np.array([t[3] for t in turns]), role=np.array([t[4] for t in turns]))
    print(os.path.basename(qdir), len(turns), flush=True)
print("GPU NAIVE DONE", flush=True)
