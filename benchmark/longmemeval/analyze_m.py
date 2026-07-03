"""Link _m retrieval recall to agentic accuracy: on questions where funes retrieved the
gold turn but naive did not, did that convert to correct answers?
"""
import json, os, glob, re, subprocess
import numpy as np
from fastembed import TextEmbedding

SP = os.environ.get("LME_WORK", ".")
FUNES = os.environ.get("FUNES_BIN", "/home/ubuntu/funes/target/release/funes")
GET = re.compile(r"(?:->|→)\s*get\s+(\S+)\s+(\S+)")
env = dict(os.environ); env["FASTEMBED_CACHE_DIR"] = "/home/ubuntu/funes/.fastembed_cache"
model = TextEmbedding(model_name="BAAI/bge-small-en-v1.5")
dump = {r["qid"]: r for r in json.load(open(f"{SP}/agentic_dump_m.json"))}

rows = []
for qdir in sorted(glob.glob(f"{SP}/work50_m/*")):
    if not os.path.isdir(qdir):
        continue
    g = json.load(open(f"{qdir}/gold.json"))
    if g["is_abstention"] or not g["gold_turn_uuids"]:
        continue
    gturn = set(g["gold_turn_uuids"])
    e = dict(env); e["FUNES_HOME"] = f"{qdir}/store"
    r = subprocess.run([FUNES, "recall", g["question"], "--k", "10", "--half-life", "0"],
                       env=e, capture_output=True, text=True)
    fh = bool({h[1] for h in GET.findall(r.stdout)} & gturn)
    d = np.load(f"{qdir}/naive_emb.npz", allow_pickle=True)
    q = np.array(list(model.embed([g["question"]]))[0], dtype=np.float32); q /= np.linalg.norm(q) + 1e-9
    order = np.argsort(-(d["emb"] @ q))[:10]
    nh = bool({f"{d['sid'][i]}::t{d['idx'][i]}" for i in order} & gturn)
    v = dump[os.path.basename(qdir)]["verdicts"]
    rows.append((os.path.basename(qdir), fh, nh, v["funes"], v["naive"]))

fhnm = [r for r in rows if r[1] and not r[2]]   # funes retrieved gold turn, naive did not
print(f"non-abstention questions analyzed: {len(rows)}")
print(f"\nfunes-retrieved-gold-turn AND naive-missed: {len(fhnm)} questions")
for qid, fh, nh, fv, nv in fhnm:
    print(f"  {qid}: funes_answer_correct={fv}  naive_answer_correct={nv}")
print(f"\n  -> in that set: funes answered correct {sum(1 for r in fhnm if r[3] is True)}/{len(fhnm)}, "
      f"naive correct {sum(1 for r in fhnm if r[4] is True)}/{len(fhnm)}")
# reverse set
nhfm = [r for r in rows if r[2] and not r[1]]
print(f"\nnaive-retrieved-gold-turn AND funes-missed: {len(nhfm)} questions")
for qid, fh, nh, fv, nv in nhfm:
    print(f"  {qid}: funes_correct={fv} naive_correct={nv}")
