#!/bin/sh
# `setup add` registers the bundle with pi as an extension and leaves the memory in the file
# index.ts reads; `setup remove` unregisters it, a pre-registry install with it. Run as funes runs
# it: from the bundle's place in the registry, with the contract's environment, against a `pi` that
# only records what it is asked.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=$(CDPATH= cd -- "$HERE/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "pi setup: $*" >&2
    exit 1
}

export HOME="$tmp/home"
mkdir -p "$HOME/.funes/agents" "$tmp/bin"
cat >"$tmp/bin/pi" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"$FUNES_TEST_CLI_LOG"
FAKE
chmod +x "$tmp/bin/pi"
# The JS runtime the seed converts with, found before PATH is cut down to the fakes.
node=$(command -v node) || fail "no node to seed with"
export PATH="$tmp/bin:/usr/bin:/bin"
export FUNES_TEST_CLI_LOG="$tmp/cli.log"
export FUNES_BIN=funes FUNES_HOME="$HOME/.funes" FUNES_AGENT_ID=pi
unset PI_CODING_AGENT_DIR PI_CODING_AGENT_SESSION_DIR
# A session pi already wrote — the converter's fixture — for the seed to find, and the JS runtime
# the seed converts it with, on PATH by way of a shim so it can be taken away.
sessions="$HOME/.pi/agent/sessions"
mkdir -p "$sessions/--Users-me-repo--"
cp "$BUNDLE/test/session.jsonl" "$sessions/--Users-me-repo--/session.jsonl"
printf '#!/bin/sh\nexec "%s" "$@"\n' "$node" >"$tmp/bin/node"
chmod +x "$tmp/bin/node"

cp -R "$BUNDLE" "$HOME/.funes/agents/pi"
dir="$HOME/.funes/agents/pi"
setup="$dir/setup"

"$setup" add acme/kb >"$tmp/add.out"
grep -q "installed funes into pi" "$tmp/add.out" || fail "add did not report: $(cat "$tmp/add.out")"
[ -f "$dir/index.ts" ] || fail "the extension itself is missing"
[ "$(cat "$dir/memory")" = acme/kb ] || fail "no memory file for index.ts"
[ "$(cat "$dir/spool")" = "$FUNES_HOME/spool/pi" ] || fail "the spool is not recorded"
[ "$(cat "$dir/sessions")" = "$sessions" ] || fail "the sessions root is not recorded"
[ -f "$FUNES_HOME/spool/pi/session.funes.jsonl" ] || fail "the history was not seeded"
[ -x "$setup" ] && [ -x "$dir/scripts/funes-index.sh" ] || fail "the scripts are not executable"
# The version gate, then the registration.
expected="--version
install $dir"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "pi was asked:
$(cat "$FUNES_TEST_CLI_LOG")"

# Re-running binds the local memory: the file index.ts reads is gone, not left stale. And with no
# JS runtime to seed with, the history is left for the extension to convert at its next start.
rm "$tmp/bin/node" "$FUNES_HOME/spool/pi/session.funes.jsonl"
"$setup" add >/dev/null 2>"$tmp/add.err"
[ ! -e "$dir/memory" ] || fail "the memory file was left stale"
grep -q "no JS runtime" "$tmp/add.err" || fail "the seed did not say it was left pending: $(cat "$tmp/add.err")"
[ "$(cat "$dir/seed-pending")" = "$sessions" ] || fail "the history was not left for the extension"

: >"$FUNES_TEST_CLI_LOG"
"$setup" remove >/dev/null
[ ! -e "$dir/spool" ] || fail "the spool record stays"
[ ! -e "$dir/sessions" ] && [ ! -e "$dir/seed-pending" ] || fail "the records stay"
[ ! -e "$FUNES_HOME/spool/pi" ] || fail "the spool stays"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "remove $dir" ] || fail "pi was asked:
$(cat "$FUNES_TEST_CLI_LOG")"
# Already absent remains a successful no-op.
"$setup" remove >/dev/null

# With no `pi` on PATH, a pre-registry install still goes, and the rest is spelled out.
legacy="$HOME/.funes/integrations/pi"
mkdir -p "$legacy"
printf extension >"$legacy/index.ts"
PATH=/usr/bin:/bin "$setup" remove >"$tmp/remove.out"
grep -q "remove the registration manually" "$tmp/remove.out" || fail "no manual step: $(cat "$tmp/remove.out")"
[ ! -e "$legacy" ] || fail "the pre-registry install stays without pi"
echo "pi setup: ok"
