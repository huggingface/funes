#!/usr/bin/env python3
"""Convert a LongMemEval haystack into a funes-indexable parquet corpus.

Reads one of the LongMemEval JSON files (xiaowu0162/longmemeval on the HF Hub)
and emits a parquet with one row per unique haystack session, in the trace
schema `funes index <file>.parquet` ingests: `session_id` / `sent_at` /
`messages` (a list of JSON-encoded OpenAI-style chat messages).

Session ids repeat across questions with identical content; only the dates
differ (each question re-stamps its haystack to build its own timeline). The
corpus keeps one copy per id with the earliest date seen, so recency ordering
stays coherent within the store. Per-question timelines — and the turn-level
`has_answer` labels — are a property of the questions, not the corpus; they
are emitted alongside as qrels for the retrieval eval.

Usage:
    build_corpus.py <longmemeval_json> <out_dir>

Writes <out_dir>/<stem>.parquet and <out_dir>/<stem>.qrels.json. The parquet
file stem becomes the store's project facet.
"""

import json
import sys
from datetime import datetime, timezone
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq


def to_rfc3339(date: str) -> str:
    """LongMemEval dates ('2023/05/20 (Sat) 02:21') → RFC3339 UTC."""
    day, hm = date.split(" (")[0], date.rsplit(" ", 1)[1]
    dt = datetime.strptime(f"{day} {hm}", "%Y/%m/%d %H:%M")
    return dt.replace(tzinfo=timezone.utc).isoformat().replace("+00:00", "Z")


def answer_turns(q: dict) -> list[dict]:
    """`{session_id, seq}` for each answer-bearing turn, seq counted as funes
    assigns it on ingest: only turns with non-blank content are retained."""
    out = []
    for sid, sess in zip(q["haystack_session_ids"], q["haystack_sessions"]):
        if sid not in q["answer_session_ids"]:
            continue
        seq = 0
        for t in sess:
            if not t["content"].strip():
                continue
            if t.get("has_answer"):
                out.append({"session_id": sid, "seq": seq})
            seq += 1
    return out


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    src, out_dir = Path(sys.argv[1]), Path(sys.argv[2])
    data = json.loads(src.read_text())
    out_dir.mkdir(parents=True, exist_ok=True)

    sessions: dict[str, tuple[str, list[str]]] = {}  # id -> (sent_at, messages)
    qrels = []
    for q in data:
        for sid, date, sess in zip(
            q["haystack_session_ids"], q["haystack_dates"], q["haystack_sessions"]
        ):
            ts = to_rfc3339(date)
            msgs = [
                json.dumps({"role": t["role"], "content": t["content"]})
                for t in sess
            ]
            if sid not in sessions or ts < sessions[sid][0]:
                sessions[sid] = (ts, msgs)
        qrels.append(
            {
                "question_id": q["question_id"],
                "question_type": q["question_type"],
                "question": q["question"],
                "answer": q["answer"],
                "question_date": q["question_date"],
                "haystack_session_ids": q["haystack_session_ids"],
                "answer_session_ids": q["answer_session_ids"],
                # seq of each answer-bearing turn within its session, matching
                # the `<session_id>-<seq>` turn uuids funes assigns on ingest —
                # blank-content turns are dropped there and don't advance seq.
                "answer_turns": answer_turns(q),
            }
        )

    ids = sorted(sessions)
    table = pa.table(
        {
            "session_id": pa.array(ids, pa.string()),
            "sent_at": pa.array([sessions[i][0] for i in ids], pa.string()),
            "messages": pa.array(
                [sessions[i][1] for i in ids], pa.list_(pa.string())
            ),
        }
    )
    stem = src.stem if src.suffix else src.name
    pq.write_table(table, out_dir / f"{stem}.parquet")
    (out_dir / f"{stem}.qrels.json").write_text(json.dumps(qrels, indent=1))
    n_turns = sum(len(m) for _, m in sessions.values())
    print(
        f"{stem}: {len(data)} questions, {len(ids)} unique sessions, "
        f"{n_turns} messages -> {out_dir / f'{stem}.parquet'}"
    )


if __name__ == "__main__":
    main()
