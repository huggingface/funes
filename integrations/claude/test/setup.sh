#!/bin/sh
# `setup add` installs the hooks-only plugin, leaves the memory in a file beside its script, and
# registers both surfaces with Claude; `setup remove` takes them back, a pre-registry install with
# them, and never the memory. Run as funes runs it: from the bundle's place in the registry, with
# the contract's environment, against a `claude` that only records what it is asked.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=$(CDPATH= cd -- "$HERE/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "claude setup: $*" >&2
    exit 1
}

export HOME="$tmp/home"
mkdir -p "$HOME/.funes/agents" "$tmp/bin"
cat >"$tmp/bin/claude" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"$FUNES_TEST_CLI_LOG"
FAKE
chmod +x "$tmp/bin/claude"
export PATH="$tmp/bin:/usr/bin:/bin"
export FUNES_TEST_CLI_LOG="$tmp/cli.log"
export FUNES_BIN=funes FUNES_HOME="$HOME/.funes" FUNES_AGENT_ID=claude
# Claude Code's transcripts live under its config directory, wherever the user put it: the
# converter's fixture, for the seed to find.
export CLAUDE_CONFIG_DIR="$tmp/claude-config"
mkdir -p "$CLAUDE_CONFIG_DIR/projects/-Users-me-repo"
cp "$BUNDLE/claude-plugin/funes/test/session.jsonl" "$CLAUDE_CONFIG_DIR/projects/-Users-me-repo/session.jsonl"

cp -R "$BUNDLE" "$HOME/.funes/agents/claude"
setup="$HOME/.funes/agents/claude/setup"
plugin="$HOME/.funes/agents/claude/claude-plugin"
hooks="$plugin/funes/hooks/hooks.json"
run_script='sh "${CLAUDE_PLUGIN_ROOT}/scripts/funes-index.sh"'

# Bound to a memory.
"$setup" add acme/kb >"$tmp/add.out"
grep -q "installed funes into Claude Code" "$tmp/add.out" || fail "add did not report: $(cat "$tmp/add.out")"
[ -f "$plugin/.claude-plugin/marketplace.json" ] || fail "no marketplace manifest"
[ -f "$plugin/funes/.claude-plugin/plugin.json" ] || fail "no plugin manifest"
[ -x "$plugin/funes/scripts/funes-index.sh" ] || fail "the hook script is not executable"
# The plugin's own script per turn and for a sub-agent; the same script publishes at both boundaries.
[ "$(jq -r '.hooks.Stop[0].hooks[0].command' "$hooks")" = "$run_script" ] || fail "Stop: $(cat "$hooks")"
[ "$(jq -r '.hooks.SubagentStop[0].hooks[0].command' "$hooks")" = "$run_script" ] || fail "SubagentStop: $(cat "$hooks")"
[ "$(jq -r '.hooks.SessionStart[0].hooks[0].command' "$hooks")" = "$run_script --publish" ] || fail "SessionStart: $(cat "$hooks")"
[ "$(jq -r '.hooks.SessionEnd[0].hooks[0].command' "$hooks")" = "$run_script --publish" ] || fail "SessionEnd: $(cat "$hooks")"
# The memory rides in a file, not the hook command, so the hooks stay a static file.
if grep -q acme/kb "$hooks"; then fail "the memory is in the hook command"; fi
[ "$(cat "$plugin/funes/scripts/memory")" = acme/kb ] || fail "no memory file"
[ "$(cat "$plugin/funes/scripts/spool")" = "$FUNES_HOME/spool/claude" ] || fail "the spool is not recorded"
[ -f "$FUNES_HOME/spool/claude/session.funes.jsonl" ] || fail "the history was not seeded"
expected="plugin marketplace add $plugin
plugin uninstall funes@huggingface
plugin install funes@huggingface
mcp remove funes
mcp add funes -s user -- funes mcp acme/kb"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "claude was asked:
$(cat "$FUNES_TEST_CLI_LOG")"

# Re-run to bind the local memory: the file goes, and so do the boundary hooks.
"$setup" add >/dev/null
[ ! -e "$plugin/funes/scripts/memory" ] || fail "the memory file was left stale"
jq -e '.hooks | has("SessionEnd") | not' "$hooks" >/dev/null || fail "a local install publishes"

# Remove: the registrations, the spool, a pre-registry install — and never the memory.
legacy="$HOME/.funes/integrations/claude-plugin"
mkdir -p "$legacy" "$HOME/.funes/memory/chunks.lance"
printf owned >"$legacy/marker"
printf memory >"$HOME/.funes/memory/chunks.lance/keep"
: >"$FUNES_TEST_CLI_LOG"
"$setup" remove >/dev/null
[ ! -e "$legacy" ] || fail "the pre-registry install stays"
[ ! -e "$FUNES_HOME/spool/claude" ] || fail "the spool stays"
[ ! -e "$plugin/funes/scripts/spool" ] || fail "the spool record stays"
[ "$(cat "$HOME/.funes/memory/chunks.lance/keep")" = memory ] || fail "the memory was touched"
expected="mcp remove funes -s user
plugin uninstall funes@huggingface
plugin marketplace remove huggingface"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "claude was asked:
$(cat "$FUNES_TEST_CLI_LOG")"
# Already absent remains a successful no-op.
"$setup" remove >/dev/null

# With no `claude` on PATH, what is funes's own still goes, and the rest is spelled out.
mkdir -p "$legacy"
printf owned >"$legacy/marker"
PATH=/usr/bin:/bin "$setup" remove >"$tmp/remove.out"
grep -q "remove the registrations manually" "$tmp/remove.out" || fail "no manual steps: $(cat "$tmp/remove.out")"
[ ! -e "$legacy" ] || fail "the pre-registry install stays without claude"
echo "claude setup: ok"
