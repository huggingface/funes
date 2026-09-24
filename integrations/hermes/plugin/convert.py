"""Convert hermes sessions from its SQLite state store into funes turns files.

hermes keeps every session's messages in one WAL database rather than a file per session: a
`sessions` table, optionally carrying `cwd`, and a `messages` table, one row per message. One row
becomes one turn, and one session becomes one `<session id>.funes.jsonl` in funes's spool.

Reading rules that keep the index faithful and its ids stable:
  - order by `id`, never `timestamp` (hermes writes `time.time()`, which is non-monotonic);
  - read every row whatever its `active`/`compacted` state — pre-compression turns are real history,
    and `id` gives them stable ids;
  - read `content`, not `api_content`, which is a byte-fidelity API sidecar with ephemeral
    injections; the `messages_fts*` tables are search indexes and are ignored.

The store is opened read-only, so this never takes a lock and is safe while hermes runs. It uses
nothing but the Python hermes itself runs on.

    python3 convert.py <state.db> <spool-dir> [session-id]
"""

import json
import os
import sqlite3
import sys
from datetime import datetime, timezone

HARNESS = "hermes"
FORMAT = 1

# Unicode White_Space, which is what Rust's `str::trim` tests. Python's own `str.strip` differs:
# it also strips U+001C–U+001F, and a block is kept or dropped on this test.
WS = (
    "\t\n\v\f\r \u0085\u00a0\u1680"
    "\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a"
    "\u2028\u2029\u202f\u205f\u3000"
)


def is_blank(text):
    return text.strip(WS) == ""


def compact(value):
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def text_like(block_type, text):
    """A `text`/`thinking` block from non-blank text, else nothing."""
    return None if is_blank(text or "") else {"block_type": block_type, "text": text}


def tool_use_blocks(tool_calls):
    """The assistant's `tool_calls` — OpenAI's `[{id, function:{name, arguments}}]` — as tool_use
    blocks. `arguments` is already a JSON string there and is kept verbatim; an object form is
    re-serialized compactly. A blank or unparseable value yields nothing."""
    try:
        parsed = json.loads(tool_calls or "")
    except ValueError:
        return []
    if not isinstance(parsed, list):
        return []
    blocks = []
    for call in parsed:
        func = call.get("function") if isinstance(call, dict) else None
        args = func.get("arguments") if isinstance(func, dict) else None
        if isinstance(args, str):
            text = args
        elif args is None:
            text = "{}"
        else:
            text = compact(args)
        block = {"block_type": "tool_use", "text": text}
        name = func.get("name") if isinstance(func, dict) else None
        if isinstance(name, str):
            block["tool_name"] = name
        call_id = call.get("id") if isinstance(call, dict) else None
        if isinstance(call_id, str):
            block["tool_use_id"] = call_id
        blocks.append(block)
    return blocks


def blocks_for(row):
    """A row's typed blocks by role: `user` gives text; `assistant` gives thinking, then text, then
    tool_use; `tool` gives tool_result. Any other role gives none, and the turn is dropped."""
    role = row["role"] or ""
    if role == "user":
        return [b for b in [text_like("text", row["content"])] if b]
    if role == "assistant":
        # reasoning_content is the human-readable trace; reasoning is the terser fallback.
        thinking = row["reasoning_content"] or row["reasoning"] or ""
        blocks = [b for b in [text_like("thinking", thinking), text_like("text", row["content"])] if b]
        blocks.extend(tool_use_blocks(row["tool_calls"]))
        return blocks
    if role == "tool":
        block = text_like("tool_result", row["content"])
        if not block:
            return []
        if isinstance(row["tool_name"], str):
            block["tool_name"] = row["tool_name"]
        if isinstance(row["tool_call_id"], str):
            block["tool_use_id"] = row["tool_call_id"]
        return [block]
    return []


def timestamp(secs):
    """An epoch-seconds `timestamp` as the format wants it: RFC 3339 in UTC with a `Z`. A missing or
    out-of-range value yields an empty string, which recency treats as fresh."""
    if secs is None:
        return ""
    try:
        dt = datetime.fromtimestamp(float(secs), timezone.utc)
    except (OverflowError, OSError, ValueError):
        return ""
    return dt.isoformat().replace("+00:00", "Z")


def _open(db):
    return sqlite3.connect(f"file:{db}?mode=ro", uri=True)


def _has_cwd(conn):
    """hermes schema versions differ and naming a missing column is a hard SQL error, not a NULL."""
    return conn.execute("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'cwd'").fetchone() is not None


def session_ids(db):
    """Every session that has at least one message."""
    with _open(db) as conn:
        return [r[0] for r in conn.execute("SELECT DISTINCT session_id FROM messages ORDER BY session_id")]


def session_ids_since(db, since):
    """Every session with a message written after `since` (epoch seconds)."""
    with _open(db) as conn:
        rows = conn.execute(
            "SELECT DISTINCT session_id FROM messages WHERE timestamp > ? ORDER BY session_id", (since,)
        )
        return [r[0] for r in rows]


def latest_session_id(db):
    """The session whose message landed last, which is the one a turn hook just finished."""
    with _open(db) as conn:
        row = conn.execute("SELECT session_id FROM messages ORDER BY id DESC LIMIT 1").fetchone()
    return row[0] if row else None


def turns_of(db, session_id):
    with _open(db) as conn:
        conn.row_factory = sqlite3.Row
        cwd = None
        if _has_cwd(conn):
            row = conn.execute("SELECT cwd FROM sessions WHERE id = ?", (session_id,)).fetchone()
            if row is not None and isinstance(row["cwd"], str):
                cwd = row["cwd"]
        rows = conn.execute(
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp, "
            "reasoning, reasoning_content FROM messages WHERE session_id = ? ORDER BY id",
            (session_id,),
        ).fetchall()

    turns = []
    seq = 0
    for row in rows:
        blocks = blocks_for(row)
        if not blocks:
            continue
        turn = {"format": FORMAT, "session_id": session_id}
        if cwd is not None:
            turn["cwd"] = cwd
        turn["turn_uuid"] = str(row["id"])
        turn["seq"] = seq
        turn["ts"] = timestamp(row["timestamp"])
        turn["role"] = row["role"] or ""
        turn["blocks"] = blocks
        turn["harness"] = HARNESS
        turns.append(turn)
        seq += 1
    return turns


def convert(db, spool, session_id):
    """One session into `<spool>/<session id>.funes.jsonl`; returns the path written."""
    out = os.path.join(spool, f"{session_id}.funes.jsonl")
    body = "".join(compact(t) + "\n" for t in turns_of(db, session_id))
    os.makedirs(spool, exist_ok=True)
    # Atomic, and the temporary name is not a `.jsonl` one, so a concurrent `funes index` over the
    # spool neither lists it nor refuses the directory for holding it.
    tmp = f"{out}.tmp{os.getpid()}"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(body)
    os.replace(tmp, out)
    return out


def convert_all(db, spool):
    return [convert(db, spool, sid) for sid in session_ids(db)]


if __name__ == "__main__":
    if len(sys.argv) not in (3, 4):
        print("usage: convert.py <state.db> <spool-dir> [session-id]", file=sys.stderr)
        raise SystemExit(2)
    store, target = sys.argv[1], sys.argv[2]
    written = [convert(store, target, sys.argv[3])] if len(sys.argv) == 4 else convert_all(store, target)
    print(len(written))
