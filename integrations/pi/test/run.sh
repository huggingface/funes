#!/bin/sh
# The converter's acceptance test: a real pi session, trimmed and secret-scanned, and the turns file
# it must produce. `expected.funes.jsonl` is what funes's own pi parser emitted while it had one, so
# a change here that moves a chunk id re-keys sessions users already hold.
#
#   sh integrations/pi/test/run.sh [node|bun|deno]
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
JS=${1:-node}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT INT TERM

"$JS" "$HERE/../convert.mjs" "$HERE/session.jsonl" "$out"
if ! diff -u "$HERE/expected.funes.jsonl" "$out/session.funes.jsonl"; then
    echo "pi converter: output changed — see the diff above" >&2
    exit 1
fi
if [ "$out/session.funes.jsonl" -nt "$HERE/session.jsonl" ] || [ "$out/session.funes.jsonl" -ot "$HERE/session.jsonl" ]; then
    echo "pi converter: the turns file does not carry the session's own time" >&2
    exit 1
fi
echo "pi converter: ok"
