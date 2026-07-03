"""Parallel funes indexing with CPU-affinity pinning (avoids onnxruntime thread thrash).
W workers, each pinned to C distinct cores; the funes subprocess inherits the affinity.

Usage: python3 index_pinned.py <workdir> <store_root> <W> <C> [qid_list_file]
  indexes each <workdir>/<qid> into <store_root>/<qid>/store
"""
import os, sys, glob, subprocess, time
import multiprocessing as mp

FUNES = os.environ.get("FUNES_BIN", "/home/ubuntu/funes/target/release/funes")
FASTEMBED = "/home/ubuntu/funes/.fastembed_cache"
NCORES = os.cpu_count()


def worker(qdirs, cores, store_root):
    try:
        os.sched_setaffinity(0, set(cores))
    except Exception as e:
        print("affinity warn:", e)
    env = dict(os.environ)
    env["FASTEMBED_CACHE_DIR"] = FASTEMBED
    for d in qdirs:
        store = os.path.join(store_root, os.path.basename(d), "store")
        os.makedirs(store, exist_ok=True)
        e = dict(env); e["FUNES_HOME"] = store
        subprocess.run([FUNES, "index", d, "--harness", "claude"],
                       env=e, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main():
    workdir, store_root, W, C = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    if len(sys.argv) > 5:
        qids = [l.strip() for l in open(sys.argv[5]) if l.strip()]
        qdirs = [os.path.join(workdir, q) for q in qids]
    else:
        qdirs = [d for d in sorted(glob.glob(f"{workdir}/*")) if os.path.isdir(d)]
    chunks = [qdirs[i::W] for i in range(W)]
    t0 = time.time()
    procs = []
    for i in range(W):
        cores = [(i * C + j) % NCORES for j in range(C)]
        p = mp.Process(target=worker, args=(chunks[i], cores, store_root))
        p.start(); procs.append(p)
    for p in procs:
        p.join()
    dt = time.time() - t0
    print(f"indexed {len(qdirs)} questions with W={W} C={C} in {dt:.0f}s")


if __name__ == "__main__":
    main()
