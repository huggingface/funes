#!/bin/sh
# `setup add` installs the plugin carrying the skill and the hooks, leaves the memory in a file
# beside its script, registers the MCP server with Codex, and clears an install from before the
# plugin; `setup remove` takes the registrations back and prunes funes's entries out of a hooks file
# the user's own share. Run as funes runs it: from the bundle's place in the registry, with the
# contract's environment, against a `codex` that only records what it is asked.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BUNDLE=$(CDPATH= cd -- "$HERE/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
fail() {
    echo "codex setup: $*" >&2
    exit 1
}

export HOME="$tmp/home"
mkdir -p "$HOME/.funes/agents" "$tmp/bin"
cat >"$tmp/bin/codex" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >>"$FUNES_TEST_CLI_LOG"
FAKE
chmod +x "$tmp/bin/codex"
export PATH="$tmp/bin:/usr/bin:/bin"
export FUNES_TEST_CLI_LOG="$tmp/cli.log"
export FUNES_BIN=funes FUNES_HOME="$HOME/.funes" FUNES_AGENT_ID=codex
# Codex answers for its own home; the default lands in the fake one.
unset CODEX_HOME

cp -R "$BUNDLE" "$HOME/.funes/agents/codex"
setup="$HOME/.funes/agents/codex/setup"
marketplace="$HOME/.funes/agents/codex/codex-plugin"
plugin="$marketplace/plugins/funes"
hooks="$plugin/hooks.json"
run_script='sh "${PLUGIN_ROOT}/scripts/funes-index.sh"'
codex_dir="$HOME/.codex"

# What an install before the plugin left: hook entries funes wrote in Codex's own file, the scripts
# they ran, and a skill in Codex's tree and in the shared one an earlier install used.
mkdir -p "$codex_dir/hooks" "$codex_dir/skills/funes" "$HOME/.agents/skills/funes"
cat >"$codex_dir/hooks.json" <<'PRE'
{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"bash \"/old/funes-index.sh\" \"codex\""}]}]}}
PRE
printf old >"$codex_dir/hooks/funes-index.sh"
printf old >"$codex_dir/hooks/funes-sync.log"
printf stale >"$codex_dir/skills/funes/SKILL.md"
printf stale >"$HOME/.agents/skills/funes/SKILL.md"

"$setup" add acme/kb >"$tmp/add.out"
grep -q "installed funes into Codex" "$tmp/add.out" || fail "add did not report: $(cat "$tmp/add.out")"
[ -f "$marketplace/.agents/plugins/marketplace.json" ] || fail "no marketplace manifest"
[ -f "$plugin/.codex-plugin/plugin.json" ] || fail "no plugin manifest"
# The skill Codex lists before loading any tool rides in the plugin.
grep -q "name: funes" "$plugin/skills/funes/SKILL.md" || fail "no skill in the plugin"
[ -x "$plugin/scripts/funes-index.sh" ] || fail "the hook script is not executable"
[ "$(jq -r '.hooks.Stop[0].hooks[0].command' "$hooks")" = "$run_script" ] || fail "Stop: $(cat "$hooks")"
[ "$(jq -r '.hooks.SessionStart[0].hooks[0].command' "$hooks")" = "$run_script --publish" ] || fail "SessionStart: $(cat "$hooks")"
[ "$(jq -r '.hooks.SessionEnd[0].hooks[0].command' "$hooks")" = "$run_script --publish" ] || fail "SessionEnd: $(cat "$hooks")"
if grep -q acme/kb "$hooks"; then fail "the memory is in the hook command"; fi
[ "$(cat "$plugin/scripts/memory")" = acme/kb ] || fail "no memory file"
[ "$(cat "$plugin/scripts/spool")" = "$FUNES_HOME/spool/codex" ] || fail "the spool is not recorded"
# The pre-plugin install is gone: funes wrote every hook in that file, so the file goes with the
# scripts its entries ran, and both skill copies with it.
[ ! -e "$codex_dir/hooks.json" ] || fail "the pre-plugin hook entries stay"
[ ! -e "$codex_dir/hooks" ] || fail "the pre-plugin scripts stay"
[ ! -e "$codex_dir/skills/funes" ] || fail "Codex's own skill copy stays"
[ ! -e "$HOME/.agents" ] || fail "the shared tree's skill copy stays"
expected="plugin marketplace add $marketplace
plugin add funes@huggingface
mcp add funes -- funes mcp acme/kb"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "codex was asked:
$(cat "$FUNES_TEST_CLI_LOG")"

# Remove, with a pre-plugin install that shares Codex's hooks file with a hook of the user's and
# still publishes to a memory: funes's entries go — the publishing one with them — and the user's
# stay, in a file written on one line, the way an editor or a tool may leave it.
mkdir -p "$codex_dir/hooks"
cat >"$codex_dir/hooks.json" <<'PRE'
{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"make lint"}]},{"hooks":[{"type":"command","command":"bash \"/old/funes-index.sh\" \"codex\""}]}],"SessionEnd":[{"hooks":[{"type":"command","command":"bash \"/old/funes-push.sh\" \"acme/old\" \"codex\""}]}]}}
PRE
printf owned >"$codex_dir/hooks/funes-index.sh"
printf owned >"$codex_dir/hooks/funes-push.sh"
printf keep >"$codex_dir/hooks/user-hook.sh"
: >"$FUNES_TEST_CLI_LOG"
"$setup" remove >/dev/null 2>"$tmp/remove.err"
jq -e '.hooks.Stop | length == 1' "$codex_dir/hooks.json" >/dev/null || fail "funes's Stop group stays: $(cat "$codex_dir/hooks.json")"
[ "$(jq -r '.hooks.Stop[0].hooks[0].command' "$codex_dir/hooks.json")" = "make lint" ] || fail "the user's hook went"
jq -e '.hooks | has("SessionEnd") | not' "$codex_dir/hooks.json" >/dev/null || fail "still publishes: $(cat "$codex_dir/hooks.json")"
[ ! -e "$codex_dir/hooks/funes-index.sh" ] || fail "funes's script stays"
[ ! -e "$codex_dir/hooks/funes-push.sh" ] || fail "funes's push script stays"
[ -f "$codex_dir/hooks/user-hook.sh" ] || fail "the user's script went"
if grep -q "delete the groups" "$tmp/remove.err"; then fail "asked the user to prune by hand"; fi
[ ! -e "$FUNES_HOME/spool/codex" ] || fail "the spool stays"
expected="mcp remove funes
plugin remove funes@huggingface
plugin marketplace remove huggingface"
[ "$(cat "$FUNES_TEST_CLI_LOG")" = "$expected" ] || fail "codex was asked:
$(cat "$FUNES_TEST_CLI_LOG")"
# Already absent remains a successful no-op.
"$setup" remove >/dev/null
echo "codex setup: ok"
