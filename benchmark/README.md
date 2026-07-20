# Benchmarks

Two runnable examples, each `cargo run --release --example <name>`:

- **`bench_recall`** — `recall()` latency, local vs remote, cold vs warm (below).
- **`bench_index`** — `index` build time, throughput, and memory compactness (at the end).

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

The CPU stages (query embed + BGE cross-encoder rerank) are identical whatever the memory, so the
local↔remote gap is entirely the I/O path: opening the dataset, the ANN/FTS scans, and the neighbor
fetch. The embed + rerank models are loaded once in a warm-up call that is **excluded** from all
timings.

For each memory the harness reports a single **cold** call followed by the min / median / max of
`--iters` **warm** calls:

- **remote cold** — the hf-hub file cache is empty (`--cold` gives it a fresh temp cache), so the
  IVF_PQ/FTS index and the touched Lance fragments are downloaded over `hf://` on first read.
- **remote warm** — every file the query touches is now in the local hf-hub cache, so the reads are
  served from disk and warm remote lands at ≈ local.
- **local** — the same dataset on disk; there's no real cold/warm gap (the OS page cache is already
  warm and the models are loaded), so `local` is the floor the remote legs are measured against.

## Usage

Build in release (a debug build is far slower and not representative):

```sh
cargo run --release --example bench_recall -- "<query>" --remote dacorvo/funes-Glint-Research-Fable-5 --iters 5 --cold
```

The bench downloads `--remote` (through the hf-hub crate, using the token from your environment for a
private repo) to a temp dir and benchmarks that local copy against the same dataset over `hf://` —
both legs run identical data, so the gap is the I/O path, not the corpus.

### Options

| flag | default | meaning |
|------|---------|---------|
| `<query>` (positional) | `"how does recall rerank candidates"` | the text to recall |
| `--remote <spec>` | `dacorvo/funes-Glint-Research-Fable-5` | dataset to benchmark (`org/repo` or `hf://…`), used for both legs |
| `--iters <N>` | `5` | warm iterations timed per memory (after the one cold call) |
| `--cold` | off | give the remote leg a throwaway `HF_HUB_CACHE` temp dir so its cold call is a true download (your real cache is left untouched) |
| `--k <N>` | `8` | results returned |
| `--candidates <N>` | `30` | fused candidates reranked |
| `--neighbors <N>` | `1` | adjacent chunks attached per hit |

> `--cold` only relocates the hf-hub file **cache** (via `HF_HUB_CACHE`) to a temp dir for the run —
> it does not touch your real `~/.cache/huggingface/hub`, your HF token, or `HF_HOME`. The local-leg
> download writes straight to a temp dir (it bypasses the cache), so it never pre-warms the remote
> cold call.

## Reading the output

```
dataset: dacorvo/funes-Glint-Research-Fable-5   query: "how does recall rerank candidates"   k=8 candidates=30 neighbors=1   warm iters=5

memory    cold(ms)   warm_lo  warm_med   warm_hi  hits
local       5455.9    5682.6    5956.0    6093.4     8
remote     10663.1    6504.0    6589.1    6668.6     8

remote vs local:  2.0× slower cold,  1.1× slower warm (median),  1.1× warm best-case
```

_Captured on a Mac M2 (24 GB), release build._

Both legs run the same ≈21.6k-chunk dataset (`hits` matches, confirming equal work). Remote **cold**
pays a one-time download of the index + touched Lance fragments into the hf-hub file cache (the
premium over warm). Every **warm** call is then served from that local cache, so warm remote lands at
**≈ local** (1.1×) — the per-call `hf://` I/O is gone. `warm best-case` (`warm_lo`) is the most stable
factor; `warm_hi` spikes are page-cache/GC noise at low `--iters`.

**Absolute numbers are host-dependent — read the ratio, not the floor.** Recall is dominated by the
cross-encoder rerank (`--candidates` query/passage pairs), identical work on both legs: the Mac M2
above reranks ~5–6 s for 30 candidates on CPU, whereas a Linux box with a GPU does it in well under a
second (local ≈ remote-warm ≈ ~1.9 s). The floor moves with the hardware; the remote-vs-local
**ratio** — warm ≈ local, cold a one-time download — is what the benchmark measures.

## Caveats

- It times **one query string**, repeated. Rerank load is fixed (always `--candidates`), but embed
  and ANN/FTS selectivity vary by query — run a few representative queries (short/long, common/rare
  terms) for a sturdier picture.
- For a private dataset, a token must be available (`HF_TOKEN` or the cached login) — the same one
  recall uses.

## Building a benchmark memory

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

`funes index <file>.parquet` indexes the whole file as a bulk import (one append, so the memory stays
compact); see `src/traces.rs`. The push gate redacts/holds back any rows containing secrets before
upload.

## `bench_index` — index build

`bench_index.rs` times an `index` build into a throwaway `$FUNES_HOME` (your real memory and config
are untouched; no remote is attached there, so nothing is pushed) and reports build time, embedding
throughput, and how compact the resulting memory is.

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
memory size:      53 MB
lance fragments:  1
```

The three counts are deliberately distinct granularities: **4665 sessions** chunk into **21767
chunks**, all written into **1 Lance fragment** (a physical data file). `lance fragments: 1` confirms
the bulk-import path stayed compact — a regression to per-session appends would show one fragment per
session and a much larger memory. (That run indexed the whole file; pass a large `--sessions` to do
likewise — the default 500 builds far faster.) Elapsed includes the one-time embedding-model load,
so throughput is a slight under-estimate on small inputs.

