#!/bin/sh
# The converter's acceptance test: a synthetic hermes store carrying one of each row the mapping
# treats differently — a user message, an assistant with reasoning and a tool call, a tool result, a
# `system` row and a blank one, both of which produce no turn, and a message whose time is out of
# range, which takes the previous one — and the turns file it must produce.
#
# `expected.funes.jsonl` is what funes's own hermes reader emitted while it had one, in every field
# but `ts`: that reader wrote RFC 3339 with a `+00:00` offset, which a turns file may not carry, so
# the converter writes the same instant with a `Z`. No chunk id depends on `ts`.
#
#   sh integrations/hermes/plugin/test/run.sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT INT TERM

python3 "$HERE/../convert.py" "$HERE/state.db" "$out" 20260101_000000_fixture >/dev/null
if ! diff -u "$HERE/expected.funes.jsonl" "$out/20260101_000000_fixture.funes.jsonl"; then
    echo "hermes converter: output changed — see the diff above" >&2
    exit 1
fi
# The turns file carries the session's own time: that of its last message.
if ! python3 - "$HERE/state.db" "$out/20260101_000000_fixture.funes.jsonl" <<'EOF'
import os, sqlite3, sys
db, out = sys.argv[1:]
latest = sqlite3.connect(f"file:{db}?mode=ro", uri=True).execute(
    "SELECT MAX(timestamp) FROM messages WHERE session_id = '20260101_000000_fixture'"
).fetchone()[0]
sys.exit(0 if abs(os.path.getmtime(out) - latest) < 1 else 1)
EOF
then
    echo "hermes converter: the turns file does not carry the session's own time" >&2
    exit 1
fi
# A session id names the file it becomes, so one that is not a plain name is refused.
if ! python3 - "$HERE/../convert.py" "$HERE/state.db" "$out" <<'PY'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("convert", sys.argv[1])
convert = importlib.util.module_from_spec(spec)
spec.loader.exec_module(convert)
for bad in ("../escape", "a/b", "", ".hidden"):
    try:
        convert.convert(sys.argv[2], sys.argv[3], bad)
    except ValueError:
        continue
    sys.exit(f"converted a session id that is not a file name: {bad!r}")
PY
then
    echo "hermes converter: a session id that is not a file name was written" >&2
    exit 1
fi
echo "hermes converter: ok"
