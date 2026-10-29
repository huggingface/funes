# Benchmarks

Four runnable examples, each `cargo run --release --example <name>`:

- **`bench_recall`** — `recall()` latency, local vs remote, cold vs warm.
- **`bench_backends`** — latency and output agreement between the inference backends.
- **`score_recall`** — `funes recall` scored against a labeled set of questions.
- **`sweep_rerank`** — how much reranking helps at each pool size, on a `dacorvo/funes-recall-benchmark`
  set.

## `bench_recall` — recall latency

`bench_recall.rs` times the full `recall()` call over **one dataset, local vs remote and cold vs
warm**, so you can see what the remote (`hf://`) tier costs. To keep it apples-to-apples it
downloads the `--remote` dataset to a temp dir and benchmarks that local copy against the same
dataset over `hf://` — both legs run identical data, so the gap is the I/O path, not the corpus.

## What it measures

Every timed call runs the recall pipeline:

```
embed query → vector ANN + BM25 FTS (fused by RRF) → [cross-encoder rerank] → neighbors → format
```

The rerank runs whenever the pool holds more rows than `--k`, so a default timed call (8 of 30)
reranks 30 pairs. `--candidates 8` times a call without one.

The CPU stages (query embed, and the rerank when it runs) are identical whatever the memory, so the
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

**Absolute numbers are host-dependent, so read the ratio, not the floor.** A reranked recall is
dominated by the cross-encoder (`--candidates` query/passage pairs), identical work on both legs, and
that floor moves with the CPU and the inference backend. What the benchmark measures is the remote-vs-local
**ratio**: warm close to local, cold a one-time download.

## Caveats

- It times **one query string**, repeated. Rerank load is fixed (always `--candidates`), but embed
  and ANN/FTS selectivity vary by query — run a few representative queries (short/long, common/rare
  terms) for a sturdier picture.
- For a private dataset, a token must be available (`HF_TOKEN` or the cached login) — the same one
  recall uses.

## `bench_backends` — inference backend comparison

`bench_backends.rs` compares every compiled `Embedder`/`Reranker` implementation. Build with both
the default BLAS backend and the optional ONNX reference:

```sh
cargo run --release --features onnx --example bench_backends
cargo run --release --features onnx --example bench_backends -- intfloat/multilingual-e5-small
```

It runs seven fixed workloads: 16 short documents to expose per-call overhead; 30 documents near
the 512-token truncation limit to approximate recall's rerank worst case; a ragged batch of 2 capped
documents among 14 short ones; 32 and 100 documents whose token lengths are the quantiles of real
chunks (100 is recall's pool at `--candidates 100`); and 256-document mixed and real batches, the
production batch size, which show how a backend scales and what it spends on padding.

Rerankers are compared within a model: gte-reranker-modernbert-base on ONNX, BLAS and, on macOS,
Metal, and bge-reranker-base on BLAS as the latency baseline. Each reranker's startup comes first: how
long it took to load, then its first call on the 32 real documents, which a one-shot `funes recall`
pays in full. Then each workload's latency, and agreement with the first backend of the same model:

- `cos↔ref` is the minimum cosine similarity between corresponding embedding vectors.
- `Δ↔ref` is the maximum absolute difference between corresponding reranker scores.

ONNX is the reference when that feature is present, BLAS otherwise. The one argument names the
embedding model, `BAAI/bge-small-en-v1.5` when omitted. Edit the fixed query or workloads when investigating a
particular regression. Building with only one backend still prints its timing, along with a note
that there is nothing to compare.

## `score_recall` — recall against labeled questions

`score_recall.rs` asks each question of a labeled set with `funes recall`'s defaults and prints the
receipt: the funes version, the memory's revision and the file's hash, then hit@1, hit@5, found@8,
the mean reciprocal rank and the median latency, overall and by kind, then each question's rank.
`--candidates` asks from a deeper pool. `--candidates 8` scores the fused order alone, since a pool
of the hits is not reranked.
A question is one JSON line with an `id`, the `question`, `gold` (the spans its answer is in, each a
`session_id` with optional `from` and `to` turns) and an optional `kind`. Its rank is the position of
its first hit in a gold span. The labeled sets and their receipts live in the
[`dacorvo/funes-recall-benchmark`](https://huggingface.co/datasets/dacorvo/funes-recall-benchmark)
dataset:

```sh
cargo run --release --example score_recall -- funes-memory/questions.jsonl --memory huggingface/funes-memory
```

The hits come from the call `funes recall` makes, not from its printed text: a hit's text can quote
an earlier recall, `→ get` lines included, and nothing in the text tells a quoted hit from a real one.
A run stops on a degraded read (the Hub unreachable, recall answering from the local memory), since
those hits were drawn from another memory.

## `sweep_rerank` — how much reranking helps, per pool size

`sweep_rerank.rs` measures how much a rerank helps for different pool sizes, on a set of the
[`dacorvo/funes-recall-benchmark`](https://huggingface.co/datasets/dacorvo/funes-recall-benchmark)
dataset's questions: for every question it dumps the fused order of each pool and the reranker's
score of each candidate, and the dataset's scorers compare the fused order with the reranked order
at any cut.

```sh
cargo run --release --example sweep_rerank -- questions.jsonl --memory mem/funes-memory --out sweep.jsonl
```

For each question and each `--pools` size N (10, 12, 16, 20, 24, 30, 32, 50 and 100 by default) it
asks `recall_hits` for N hits from N candidates, the fused order of the pool recall would cut, and
scores every candidate with funes's own reranker. One JSON line per question and pool: the
candidates in fused order, each with its fused score and its reranker score, and the retrieval and
rerank times. By default a question's union of pools is scored in one reranker call and a chunk's
score looked up by its id (on the GPU, a pair scored the same in every batch that held it, on the
120 pairs checked). `--per-pool` scores each pool as its own call, as recall does, twice, timing
the second so no GPU graph is built inside the timing, and `--cpu` also times the CPU reranker. The output is appended to, and a (question, pool) already
in it is skipped, so an interrupted run resumes. The scorers and the receipts live in the dataset.
