"""LongMemEval -> funes ingest adapter.

Converts each LongMemEval question's haystack into a Claude-style JSONL tree that
`funes index --harness claude` ingests, preserving the benchmark's session id
(the file stem) and turn id (each record's `uuid`) so a recall hit's
`-> get <session_id> <turn_uuid>` line maps 1:1 back onto the gold labels.
Emits gold.json per question for scoring.

Output layout, one dir per question:
  <outdir>/<question_id>/
    projects/lme/<session_id>.jsonl   # one file per haystack session, one line per turn
    gold.json                         # question, answer, gold session/turn ids, metadata

Usage: python3 adapter.py <longmemeval.json> <outdir>
  <longmemeval.json>  a LongMemEval file (JSON array of questions), e.g. longmemeval_s_cleaned.json
"""
import json, os, re, sys

_DATE = re.compile(r"(\d{4})/(\d{2})/(\d{2}).*?(\d{2}):(\d{2})")


def parse_date(s):
    # "2023/05/20 (Sat) 02:21" -> RFC3339 "2023-05-20T02:21:00Z"
    m = _DATE.match(s or "")
    if m:
        y, mo, d, h, mi = m.groups()
        return f"{y}-{mo}-{d}T{h}:{mi}:00Z"
    return "2023-01-01T00:00:00Z"


def turn_uuid(sid, ti):
    return f"{sid}::t{ti}"


def build_question(q, outdir):
    qid = q["question_id"]
    qdir = os.path.join(outdir, qid)
    proj = os.path.join(qdir, "projects", "lme")
    os.makedirs(proj, exist_ok=True)

    sessions = q["haystack_sessions"]
    sids = q["haystack_session_ids"]
    dates = q["haystack_dates"]
    gold_turns = []

    for si, sess in enumerate(sessions):
        sid = sids[si]
        ts = parse_date(dates[si])
        prev = None
        with open(os.path.join(proj, f"{sid}.jsonl"), "w") as f:
            for ti, turn in enumerate(sess):
                uid = turn_uuid(sid, ti)
                rec = {
                    "type": turn["role"],  # user | assistant
                    "uuid": uid,
                    "timestamp": ts,
                    "message": {"role": turn["role"], "content": turn["content"]},
                }
                if prev is not None:
                    rec["parentUuid"] = prev
                f.write(json.dumps(rec) + "\n")
                prev = uid
                # has_answer is present-only-when-true in _s, an explicit bool in oracle
                if turn.get("has_answer"):
                    gold_turns.append(uid)

    gold = {
        "question_id": qid,
        "question": q["question"],
        "answer": q["answer"],
        "question_type": q["question_type"],
        "question_date": q.get("question_date"),
        "is_abstention": qid.endswith("_abs"),
        "gold_session_ids": sorted(set(q["answer_session_ids"])),
        "gold_turn_uuids": gold_turns,
        "n_sessions": len(sessions),
        "n_turns": sum(len(s) for s in sessions),
    }
    json.dump(gold, open(os.path.join(qdir, "gold.json"), "w"), indent=2)
    return qdir, gold


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    inp, outdir = sys.argv[1], sys.argv[2]
    qs = json.load(open(inp))
    os.makedirs(outdir, exist_ok=True)
    for q in qs:
        _, g = build_question(q, outdir)
        print(f"{g['question_id']}: {g['n_sessions']} sessions, {g['n_turns']} turns, "
              f"gold_sessions={g['gold_session_ids']}, gold_turns={len(g['gold_turn_uuids'])}"
              f"{'  [ABSTENTION]' if g['is_abstention'] else ''}")


if __name__ == "__main__":
    main()
