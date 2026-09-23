#!/bin/sh
# The converter's acceptance test: a real Claude Code transcript, trimmed and secret-scanned, and
# the turns file it must produce. `expected.funes.jsonl` is what funes's own Claude parser emitted
# while it had one, so a change here that moves a chunk id re-keys sessions users already hold.
#
#   sh <plugin>/test/run.sh
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT INT TERM

"$HERE/../convert" "$HERE/session.jsonl" "$out/got.funes.jsonl"
if diff -u "$HERE/expected.funes.jsonl" "$out/got.funes.jsonl"; then
    echo "claude converter: ok"
else
    echo "claude converter: output changed — see the diff above" >&2
    exit 1
fi
