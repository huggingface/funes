#!/bin/sh
# The converter's acceptance test: a synthetic hermes store carrying one of each row the mapping
# treats differently — a user message, an assistant with reasoning and a tool call, a tool result, a
# `system` row and a blank one, both of which produce no turn — and the turns file it must produce.
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
if diff -u "$HERE/expected.funes.jsonl" "$out/20260101_000000_fixture.funes.jsonl"; then
    echo "hermes converter: ok"
else
    echo "hermes converter: output changed — see the diff above" >&2
    exit 1
fi
