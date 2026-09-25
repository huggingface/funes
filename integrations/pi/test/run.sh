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
# To the second: the runtime stamps to the millisecond, and ordering the spool needs no more.
if ! "$JS" "$HERE/same-second.mjs" "$out/session.funes.jsonl" "$HERE/session.jsonl"; then
    echo "pi converter: the turns file does not carry the session's own time" >&2
    exit 1
fi
echo "pi converter: ok"

# The extension's startup, run as pi runs it: node strips the types itself from 22.18 on; bun and
# deno always did.
case "$JS" in
node) TS=--experimental-strip-types ;;
*) TS= ;;
esac
if ! "$JS" $TS "$HERE/startup.mjs"; then
    echo "pi startup: a history setup left pending is not indexed at the next start — see above" >&2
    exit 1
fi
echo "pi startup: ok"
