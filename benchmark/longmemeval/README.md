# LongMemEval harness

Harness for evaluating funes on [LongMemEval](https://github.com/xiaowu0162/LongMemEval)
(arXiv:2410.10813), a cross-session long-term-memory benchmark. This directory holds the ingest
adapter and a retrieval smoke test.

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
