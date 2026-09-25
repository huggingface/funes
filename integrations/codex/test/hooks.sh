#!/bin/sh
# The per-turn hook converts the rollout its payload names, then every rollout changed since the
# hook last ran — the sessions whose own hook never fired — and a boundary converts before it
# publishes. Drives the worker half of funes-index.sh against a fake Codex home, with a `funes`
# that only records what it is asked, so the conversion alone is under test.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=$(CDPATH= cd -- "$HERE/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "codex hooks: $*" >&2
    exit 1
}

export HOME="$tmp/home"
mkdir -p "$HOME/.funes/agents" "$tmp/bin"
cat >"$tmp/bin/funes" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"$FUNES_TEST_CLI_LOG"
FAKE
chmod +x "$tmp/bin/funes"
export PATH="$tmp/bin:/usr/bin:/bin"
export FUNES_TEST_CLI_LOG="$tmp/cli.log"
unset CODEX_HOME

# Installed, with the `spool` record `setup add` writes beside the scripts.
cp -R "$BUNDLE" "$HOME/.funes/agents/codex"
scripts="$HOME/.funes/agents/codex/codex-plugin/plugins/funes/scripts"
spool="$tmp/spool"
mkdir -p "$spool"
printf '%s\n' "$spool" >"$scripts/spool"
tree="$HOME/.codex/sessions/2026/09/24"
mkdir -p "$tree"
fixture="$BUNDLE/codex-plugin/plugins/funes/test/session.jsonl"

# A rollout written now: `cp` would keep the fixture's stamp, and the sweep goes by stamps.
rollout() { cat "$fixture" >"$tree/$1"; }
# The worker half, as the foreground half runs it: the payload, then `--publish` at a boundary.
fire() { sh "$scripts/funes-index.sh" --worker "{\"transcript_path\":\"$tree/rollout-a.jsonl\",\"session_id\":\"s\"}" "${1:-}"; }
# Filesystems stamp to the second at worst, and `find -newer` compares stamps.
tick() { sleep 2; }

# Two sessions written after the install: the hook fires for one of them only.
tick
rollout rollout-a.jsonl
rollout rollout-b.jsonl
fire
[ -f "$spool/rollout-a.funes.jsonl" ] || fail "the rollout the payload named was not converted"
[ -f "$spool/rollout-b.funes.jsonl" ] || fail "the rollout whose hook never fired was not converted"
[ -f "$scripts/swept" ] || fail "the sweep left no mark"

# A third, written after that run; the second, drained by funes meanwhile, is not re-emitted.
tick
rollout rollout-c.jsonl
rm "$spool/rollout-b.funes.jsonl"
fire
[ -f "$spool/rollout-c.funes.jsonl" ] || fail "a rollout written since the last sweep was not converted"
[ ! -e "$spool/rollout-b.funes.jsonl" ] || fail "a rollout unchanged since the last sweep was converted again"

# A boundary: what was written since is converted first, then the index and the push to the
# memory recorded beside the scripts.
tick
rollout rollout-d.jsonl
printf 'acme/kb\n' >"$scripts/memory"
fire --publish
[ -f "$spool/rollout-d.funes.jsonl" ] || fail "the boundary did not convert before publishing"
expected="index --harness codex
index --harness codex
index --harness codex
push acme/kb"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "funes was asked:
$(cat "$FUNES_TEST_CLI_LOG")"
echo "codex hooks: ok"
