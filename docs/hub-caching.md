# How `hf://` recall caching works — and why it's file-based

## TL;DR

funes caches the **whole immutable files** it reads from a remote lance store into hf-hub's standard
local cache, pinned to the dataset's head commit. Warm recalls then read from local disk with no
network. The cache is **file-grained** — not byte-range, not Xet-chunk — because that's the unit
lance stores in, and the Hub delivers whole files regardless.

## What a remote store actually is

A funes remote store is a **lance dataset**: a directory of **immutable files** on the Hub.

```
chunks.lance/
  data/*.lance          data fragments      (funes-store: 8 fragments, largest 89 / 42 / 23 MB)
  _indices/**           IVF/PQ + FTS index  (~0.5–10 MB per file, a few dozen files)
  _versions/*.manifest  one manifest per dataset version (a few KB each)
```

Lance reads this through an `object_store` abstraction: it **lists `_versions/`** to find the latest
version, reads that manifest, then reads index and data files (whole, or by byte range). Every file
is addressed by path and, once written, **never changes** — new data lands as *new* files in a new
version.

## The problem

Lance's HF backend (opendal) adds **no cache**: every recall re-fetches the index and the touched
data over `hf://`. On the Hub each range read also pays a fixed ~0.3 s signed-redirect tax, so a cold
recall is hundreds of small round-trips — tens of seconds to minutes — and it is **re-paid in full on
every recall** (nothing is kept locally).

## The design: a file-grained read-through cache

On a remote open, funes:

1. **resolves the head commit SHA** (one cheap call) and pins reads to it;
2. **wraps lance's object store** so every **file read** (`get`) is served by hf-hub's
   `download_file` — which fetches the **whole file once** into the standard HF cache
   (`~/.cache/huggingface/hub`) and returns a local path, from which the requested bytes are served.
   The files lance reads this way are all **immutable** (`data/**`, `_indices/**`,
   `_versions/*.manifest`), so serving them from a commit-pinned cache is always correct;
3. **delegates listing live** — the `_versions/` listing that discovers the latest version goes
   straight to the backend (never the cache). That, together with **re-resolving the head SHA on
   every open** (step 1 runs per command), is what keeps **new pushes always picked up** on the next
   command.

Because the files are immutable and reads are pinned to a commit SHA, a warm read is a pure
local-filesystem hit — **zero network**. hf-hub owns the cache (atomic writes, cross-process locks,
`hf cache` for inspection/GC); funes adds no cache machinery of its own.

```
recall → lance → [funes wrapper]      (head SHA re-resolved once per open → pins the gets below)
                   ├─ list _versions/                → live hf://   (find the latest version)
                   └─ get manifest / _indices / data → hf-hub download_file (whole file, pinned SHA)
                                                        → ~/.cache/huggingface/hub
                                                        → warm = zero network
```

## Why file-grained — and not the Xet *chunk* cache

Xet (the Hub's content-addressed backend) stores files as sub-file *xorbs* and keeps its own chunk
cache. It is tempting to assume a lower-level cache must be finer, and therefore better. For a lance
store it is not, for two reasons:

- **No granularity win.** A xorb caps at **64 MiB**, and our files are the *same order of magnitude* —
  index files ≤10 MB, data fragments up to 89 MB (one to two xorbs). So the chunk cache's unit is
  already ~the whole file: caching at xorb grain gives no finer locality than caching whole files, it
  just adds moving parts. This holds at *any* of our file sizes — even the largest fragment is only a
  couple of xorbs, so there is no useful sub-file structure for a chunk cache to exploit that a file
  cache doesn't.
- **No dedup win.** The chunk cache's real value is reusing identical chunks *across* files. Lance
  fragments are distinct columnar data with negligible cross-file overlap, so chunk-level dedup buys
  essentially nothing here.

Lance is also **file-oriented**: it reads files by path, so the natural, trivially-correct cache key
is *(repo, commit, file path)* — exactly a file cache. So the cache wants to be **file-based, because
lance is file-based.**

> **Cache grain and read pattern are separate axes — don't conflate them.** Everything above is about
> cache *grain*: whole file, not xorb. Whether we *read* whole files or byte ranges is a different
> question with a different answer — we read whole files because of **latency**, not granularity. On
> the Hub every range read pays a fixed per-request signed-redirect cost regardless of size (see *The
> problem*), so one whole-file transfer beats a scan's hundreds of small range requests. In short:
> granularity is what makes the *chunk cache* pointless here; latency is what makes *byte-range reads*
> a poor fit.

## Why hf-hub's cache specifically

Lance implements no cache of its own and exposes no hook to populate one. hf-hub already ships exactly
the file cache we need — whole-file `download_file` → the standard HF cache, with zero-network reads
at a pinned commit SHA, plus atomic writes, locking, and `hf cache` tooling. So funes routes its
immutable-file reads through hf-hub rather than building (and having to get right) a bespoke cache.
