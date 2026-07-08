# LongMemEval harness

Harness for evaluating funes on [LongMemEval](https://github.com/xiaowu0162/LongMemEval)
(arXiv:2410.10813), a cross-session long-term-memory benchmark. This directory holds the ingest
adapter, a corpus-wide store builder, a retrieval smoke test, and the full evaluation harness
(push-RAG and agentic funes-vs-naive-dense-baseline).

## Data

`xiaowu0162/longmemeval-cleaned` (MIT, public — no auth):

```sh
# _s variant (~277 MB, 500 questions)
curl -L "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json" -o longmemeval_s_cleaned.json
# oracle (evidence-only, ~15 MB, same schema)
curl -L "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_oracle.json" -o longmemeval_oracle.json
```

Each file is a JSON array of question objects: `question_id` (ends in `_abs` for abstention),
`question_type`, `question`, `answer`, `question_date`, three index-aligned lists
(`haystack_sessions` — each a list of `{role, content, has_answer?}` turns —
`haystack_session_ids`, `haystack_dates`), and `answer_session_ids` (session-level gold). Turn-level
gold is `has_answer: true` on evidence turns.

## adapter.py

```sh
python3 adapter.py longmemeval_s_cleaned.json work/
```

Writes `work/<question_id>/`:

- `projects/lme/<session_id>.jsonl` — one file per haystack session, one Claude-format record per
  turn. Session id is the file stem; turn uuid is `<session_id>::t<turn_index>`. So a recall hit's
  `→ get <session_id> <turn_uuid>` line maps back onto the benchmark's session/turn ids.
- `gold.json` — question, answer, `gold_session_ids`, `gold_turn_uuids`, and metadata.

## build_corpus.py

Builds one **corpus-wide** store (the adapter above builds one store *per question*): a parquet in
the trace schema `funes index <file>.parquet` ingests (`session_id` / `sent_at` / `messages`),
plus a `.qrels.json` sidecar (per question: gold session ids, and `answer_turns` whose `seq`
matches the `<session_id>-<seq>` turn uuids funes assigns on parquet ingest — blank-content turns
are dropped there and don't advance seq).

Session ids repeat across questions with identical content; only the dates differ, because each
question re-stamps its haystack to build its own timeline. The corpus keeps one copy per id with
the earliest date, so recency ordering stays coherent within the store. Cleaned `_s` yields
19,195 unique sessions (199,509 messages).

```sh
pip install pyarrow
python3 build_corpus.py longmemeval_s_cleaned.json corpus/
FUNES_HOME=<store-home> funes index corpus/longmemeval_s_cleaned.parquet --yes
```

`FUNES_HOME` keeps the store isolated from your real memory; the parquet file stem becomes the
store's project facet (`longmemeval_s_cleaned`).

## smoke_test.py

Per question, indexes the haystack into an isolated store and recalls the question, reporting
whether the gold session/turn appears in the top-k hits.

```sh
FUNES_BIN=../../target/release/funes \
FASTEMBED_CACHE_DIR=../../.fastembed_cache \
python3 smoke_test.py work/ 10
```

Each question uses its own `FUNES_HOME` (`work/<question_id>/store`), so runs parallelize across
questions.

## Notes

- `k` defaults to 10.
- Recall runs with `--half-life 0`: LongMemEval dates are historical, so funes's wall-clock recency
  weighting does not apply.
- Abstention questions (`question_id` ends in `_abs`) have no gold evidence.

## Full harness

Two evaluation modes, both comparing funes retrieval against a naive BGE-small dense baseline, with
a fixed reader (local GLM-4.5-Air, OpenAI-compatible at `:8001`) and a GLM-5.2 judge (HF router).

**Push-RAG** — the harness injects top-k passages into the reader prompt:
- `sampler.py` — stratified question sample.
- `index_pinned.py` — parallel funes indexing (CPU-affinity pinned, to avoid onnxruntime thrash).
- `funes_recall_all.py`, `naive_rag.py` / `naive_precompute.py` — per-question top-k retrieval (funes vs dense).
- `lme_run.py <read|judge|report> <workdir>` — build k-turn contexts, call the reader, judge, aggregate.
- `retrieval_recall.py`, `measure_tokens.py`, `analyze_m.py` — retrieval recall@k, token accounting, retrieval→accuracy linkage.

**Agentic** — the model drives its own retrieval tool via `pi`:
- funes is exposed through `funes add pi` (`recall`/`get`); `naive-rag-ext/` is a pi extension exposing a `dense_search` tool backed by `naive_search.py`.
- `gpu_naive.py` — GPU BGE-small doc-embedding precompute (transformers; needs a torch+CUDA venv).
- `pi_run.sh <funes|naive> <qid>` — one agentic run (pi + GLM, `--no-builtin-tools`, `-e <ext>`).
- `batch_agentic.sh` — full batch; `collect_judge_agentic.py <collect|report>` — judge + report accuracy and tool-use.

## Environment

Scripts read these (defaults in parentheses):
- `LME_WORK` — working dir holding `work/<qid>/…` (cwd).
- `WROOT` / `AROOT` — work-root / agentic-output-root (`$LME_WORK/work50`, `.../agentic`).
- `FUNES_BIN` (`target/release/funes`), `FASTEMBED_CACHE_DIR` — funes binary + shared model cache.
- pi: `~/.pi/agent/models.json` must define a `local` provider (`baseUrl` = the GLM server's `/v1`,
  `contextWindow` = the server's *actual* size). Load extensions with `pi -e <index.ts>` —
  auto-discovery of `.pi/extensions/` is unreliable on pi ≥ 0.80 (see repo issue on `funes add pi`).

Paths were parametrized when moving out of a scratchpad; check them against your layout before running.
Results for the GLM-4.5-Air study live in the `dacorvo/funes-longmemeval` HF dataset.
