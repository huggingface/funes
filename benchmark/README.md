# Benchmarks

Three runnable examples, each `cargo run --release --example <name>`:

- **`bench_recall`** — `recall()` latency, local vs remote, cold vs warm.
- **`bench_index`** — `index` build time, throughput, and memory compactness.
- **`bench_backends`** — latency and output agreement between the BLAS and ONNX inference backends.

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
cargo run --release --example bench_recall -- "<query>" --remote huggingface/funes-memory --iters 5 --cold
```

The bench downloads `--remote` (through the hf-hub crate, using the token from your environment for a
private repo) to a temp dir and benchmarks that local copy against the same dataset over `hf://` —
both legs run identical data, so the gap is the I/O path, not the corpus.

### Options

| flag | default | meaning |
|------|---------|---------|
| `<query>` (positional) | `"how does recall rerank candidates"` | the text to recall |
| `--remote <spec>` | `huggingface/funes-memory` | dataset to benchmark (`org/repo` or `hf://…`), used for both legs |
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

A header repeats the dataset, the query and the knobs. Then one row per memory: `cold(ms)` is the
single cold call, `warm_lo` / `warm_med` / `warm_hi` the min / median / max over the `--iters` warm
calls, and `hits` the results returned, equal on both rows when both legs did the same work. A last
line gives the remote-to-local ratios: cold, warm median and warm best-case. `warm_lo` is the most
stable of the three, and `warm_hi` spikes are page-cache noise at low `--iters`.

**Absolute numbers are host-dependent, so read the ratio, not the floor.** Recall is dominated by the
cross-encoder rerank (`--candidates` query/passage pairs), identical work on both legs, and that floor
moves with the CPU and the inference backend. What the benchmark measures is the remote-vs-local
**ratio**: warm close to local, cold a one-time download.

## Caveats

- It times **one query string**, repeated. Rerank load is fixed (always `--candidates`), but embed
  and ANN/FTS selectivity vary by query — run a few representative queries (short/long, common/rare
  terms) for a sturdier picture.
- For a private dataset, a token must be available (`HF_TOKEN` or the cached login) — the same one
  recall uses.

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

The report lists the source, the elapsed time, the sessions and chunks indexed, the throughput in
chunks per second, the memory's size on disk and its Lance fragment count. The three counts are
distinct granularities: sessions chunk into chunks, and each indexed unit is appended once, as one
Lance fragment. A parquet file is one unit however many sessions it holds, so a parquet build should
report one fragment; more would mean the bulk-import path regressed to per-session appends. A
directory of session files is one unit per session, so there the count equals the sessions. Elapsed
includes the one-time embedding-model load, so throughput is a slight under-estimate on small inputs.

## `bench_backends` — inference backend comparison

`bench_backends.rs` compares every compiled `Embedder`/`Reranker` implementation. Build with both
the default BLAS backend and the optional ONNX reference:

```sh
cargo run --release --features onnx --example bench_backends
```

It runs six fixed workloads: 16 short documents to expose per-call overhead; 30 documents near
the 512-token truncation limit to approximate recall's rerank worst case; a ragged batch of 2 capped
documents among 14 short ones; 32 documents whose token lengths are the quantiles of real
chunks; and 256-document mixed and real batches, the production batch size, which show how
a backend scales and what it spends on padding. For each backend it reports
embedding and reranking latency plus agreement with the first backend:

- `embed cos↔ref` is the minimum cosine similarity between corresponding embedding vectors.
- `rerank Δ↔ref` is the maximum absolute difference between corresponding reranker scores.

ONNX is the reference when that feature is present. The benchmark has no command-line options; edit
its fixed query or workloads when investigating a particular regression. Building with only one
backend still prints its timing, along with a note that there is nothing to compare.
