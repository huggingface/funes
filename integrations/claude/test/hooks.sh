#!/bin/sh
# The per-turn hook converts the transcript its payload names, then every transcript changed since
# the hook last ran — the sessions whose own hook never fired — and a boundary converts before it
# publishes. Drives the worker half of funes-index.sh against a fake Claude home, with a `funes`
# that only records what it is asked, so the conversion alone is under test.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=$(CDPATH= cd -- "$HERE/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "claude hooks: $*" >&2
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

# Installed, with the `spool` record `setup add` writes beside the scripts.
cp -R "$BUNDLE" "$HOME/.funes/agents/claude"
scripts="$HOME/.funes/agents/claude/claude-plugin/funes/scripts"
spool="$tmp/spool"
mkdir -p "$spool"
printf '%s\n' "$spool" >"$scripts/spool"
tree="$HOME/.claude/projects/-Users-me-repo"
mkdir -p "$tree"
fixture="$BUNDLE/claude-plugin/funes/test/session.jsonl"

# A transcript written now: `cp` would keep the fixture's stamp, and the sweep goes by stamps.
transcript() { cat "$fixture" >"$tree/$1"; }
# The worker half, as the foreground half runs it: the payload, then `--publish` at a boundary.
fire() { sh "$scripts/funes-index.sh" --worker "{\"transcript_path\":\"$tree/a.jsonl\"}" "${1:-}"; }
# Filesystems stamp to the second at worst, and `find -newer` compares stamps.
tick() { sleep 2; }

# Two sessions written after the install: the hook fires for one of them only.
tick
transcript a.jsonl
transcript b.jsonl
fire
[ -f "$spool/a.funes.jsonl" ] || fail "the session the payload named was not converted"
[ -f "$spool/b.funes.jsonl" ] || fail "the session whose hook never fired was not converted"
[ -f "$scripts/swept" ] || fail "the sweep left no mark"

# A third, written after that run; the second, drained by funes meanwhile, is not re-emitted.
tick
transcript c.jsonl
rm "$spool/b.funes.jsonl"
fire
[ -f "$spool/c.funes.jsonl" ] || fail "a session written since the last sweep was not converted"
[ ! -e "$spool/b.funes.jsonl" ] || fail "a session unchanged since the last sweep was converted again"

# A boundary: what was written since is converted first, then the index and the push to the
# memory recorded beside the scripts.
tick
transcript d.jsonl
printf 'acme/kb\n' >"$scripts/memory"
fire --publish
[ -f "$spool/d.funes.jsonl" ] || fail "the boundary did not convert before publishing"
expected="index --harness claude
index --harness claude
index --harness claude
push acme/kb"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "funes was asked:
$(cat "$FUNES_TEST_CLI_LOG")"
echo "claude hooks: ok"
