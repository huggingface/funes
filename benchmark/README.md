# Benchmarks

Two runnable examples, each `cargo run --release --example <name>`:

- **`bench_recall`** — `recall()` latency, local vs remote, cold vs warm (below).
- **`bench_index`** — `index` build time, throughput, and store compactness (at the end).

## `bench_recall` — recall latency

`bench_recall.rs` times the full `recall()` call over **one dataset, local vs remote and cold vs
warm**, so you can see what the remote (`hf://`) tier costs. To keep it apples-to-apples it
downloads the `--remote` dataset to a temp dir and benchmarks that local copy against the same
dataset over `hf://` — both legs run identical data, so the gap is the I/O path, not the corpus.

## What it measures

Every timed call runs the whole recall pipeline:

```
embed query → vector ANN + BM25 FTS (fused by RRF) → cross-encoder rerank → recency → neighbors → format
```

The CPU stages (query embed + BGE cross-encoder rerank) are identical whatever the store, so the
local↔remote gap is entirely the I/O path: opening the dataset, the ANN/FTS scans, and the neighbor
fetch. The embed + rerank models are loaded once in a warm-up call that is **excluded** from all
timings.

For each store the harness reports a single **cold** call followed by the min / median / max of
`--iters` **warm** calls:

- **remote cold** — the Xet chunk cache is empty (`--cold` gives it a fresh temp cache), so the
  IVF_PQ/FTS index and the touched Lance fragments are downloaded over `hf://`.
- **remote warm** — Xet-cached data, but `recall()` still re-opens the dataset each call (manifest +
  index-descriptor round-trips), which dominates the warm remote cost.
- **local** — the same dataset on disk; there's no real cold/warm gap (the OS page cache is already
  warm and the models are loaded), so `local` is the floor the remote legs are measured against.

## Usage

Build in release (a debug build is far slower and not representative):

```sh
cargo run --release --example bench_recall -- "<query>" --remote dacorvo/funes-bench --iters 5 --cold
```

The bench downloads `--remote` (via the `hf` CLI, using your logged-in token for a private repo) to
a temp dir and benchmarks that local copy against the same dataset over `hf://` — both legs run
identical data, so the gap is the I/O path, not the corpus.

### Options

| flag | default | meaning |
|------|---------|---------|
| `<query>` (positional) | `"how does recall rerank candidates"` | the text to recall |
| `--remote <spec>` | `dacorvo/funes-bench` | dataset to benchmark (`org/repo` or `hf://…`), used for both legs |
| `--iters <N>` | `5` | warm iterations timed per store (after the one cold call) |
| `--cold` | off | give the remote leg a throwaway `HF_XET_CACHE` temp dir so its cold call is a true download (your real cache is left untouched) |
| `--k <N>` | `8` | results returned |
| `--candidates <N>` | `30` | fused candidates reranked |
| `--neighbors <N>` | `1` | adjacent chunks attached per hit |

> `--cold` only relocates the Xet **cache** (via `HF_XET_CACHE`) to a temp dir for the run — it does
> not touch your real `~/.cache/huggingface/xet`, your HF token, or `HF_HOME`. With `--cold` the
> dataset is fetched twice: once by `hf download` for the local copy, once over Xet for the remote
> cold call.

## Reading the output

```
dataset: dacorvo/funes-bench   query: "ray traced multiplayer shooter with bots"   k=8 candidates=30 neighbors=1   warm iters=5

store     cold(ms)   warm_lo  warm_med   warm_hi  hits
local        896.9     796.1     814.6    4486.6     8
remote      9804.5    8257.2    8796.7    8900.6     8

remote vs local:  10.9× slower cold,  10.8× slower warm (median),  10.4× warm best-case
```

Both legs index the same ≈21.6k-chunk dataset (`hits` matches, confirming equal work). The headline
line reports the slowdown over the local floor: remote **cold** (~10–14 s across runs, depending on
download bandwidth) pays the full Xet download of the index + touched Lance fragments; even **warm** it's
~10× local. `warm best-case` (`warm_lo`) is the most stable factor — the `warm_hi` spikes are
page-cache/GC noise at low `--iters`. Most of the warm-remote cost is `recall()` re-opening the
dataset on every call, which a long-lived process (the MCP server) that opens once would avoid.

## Caveats

- It times **one query string**, repeated. Rerank load is fixed (always `--candidates`), but embed
  and ANN/FTS selectivity vary by query — run a few representative queries (short/long, common/rare
  terms) for a sturdier picture.
- Needs the `hf` CLI for the download (unless `--local` is given) and, for a private dataset, a
  logged-in HF token.

## Building a benchmark store

The remote target above was built from a public agent-trace dataset with funes' parquet indexer:

```sh
# 1. fetch the auto-converted parquet (one row per session)
hf download Glint-Research/Fable-5-traces --repo-type dataset \
  --revision refs/convert/parquet --include "pi_agent/train/0000.parquet" --local-dir ./traces

# 2. (optional) slice to ~N sessions to hit a target chunk count, then index into an isolated home
FUNES_HOME=./bench-home funes index ./traces/pi_agent/train/0000.parquet

# 3. publish to a Hub dataset repo you own (create it first; funes won't), which re-materializes a
#    clean, compact dataset on the remote
hf repo create <org>/<repo> --repo-type dataset
FUNES_HOME=./bench-home funes use <org>/<repo>
FUNES_HOME=./bench-home funes push
```

`funes index <file>.parquet` indexes the whole file as a bulk import (one append, so the store stays
compact); see `src/traces.rs`. The push gate redacts/holds back any rows containing secrets before
upload.

## `bench_index` — index build

`bench_index.rs` times an `index` build into a throwaway `$FUNES_HOME` (your real store and config
are untouched; no remote is attached there, so nothing is pushed) and reports build time, embedding
throughput, and how compact the resulting store is.

`--sessions <N>` caps how many sessions are indexed (default **500**) so the build doesn't run long
over a big tree or the full parquet — raise it for a longer, steadier measurement.

```sh
cargo run --release --example bench_index -- path/to/traces.parquet                 # first 500 sessions
cargo run --release --example bench_index -- ~/.claude/projects --sessions 100       # a JSONL tree, capped
cargo run --release --example bench_index -- path/to/traces.parquet --sessions 5000  # longer run
```

Example output:

```
=== index benchmark ===
source:           Fable-5-traces.parquet
elapsed:          385.0s  (incl. model load)
sessions:         4665
chunks:           21767
throughput:       57 chunks/s
store size:       53 MB
lance fragments:  1
```

The three counts are deliberately distinct granularities: **4665 sessions** chunk into **21767
chunks**, all written into **1 Lance fragment** (a physical data file). `lance fragments: 1` confirms
the bulk-import path stayed compact — a regression to per-session appends would show one fragment per
session and a much larger store. (That run indexed the whole file; pass a large `--sessions` to do
likewise — the default 500 builds far faster.) Elapsed includes the one-time embedding-model load,
so throughput is a slight under-estimate on small inputs.

