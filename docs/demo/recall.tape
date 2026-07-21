# Demo: an agent recalling from a memory.
# For the purpose of the demo, the memory is remote.
# We hide it by using a claude alias.
#
#
# Prerequisites on the render box:
#   - claude installed and authenticated to a model (auth comes from your real config;
#     --mcp-config only overrides which MCP servers load, not your login).
#   - PRE-WARM once before recording (cold starts otherwise download the model + dataset): run
#     the invocation above once so docs/demo caches the remote and the embedding model; then
#     recording starts warm and the recall lands inside the budget.
#
# Regenerate from the repo root:
#   vhs docs/demo/recall.tape

Output docs/img/recall.gif

Set FontSize 16
Set Width 800
Set Height 600
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

# Hidden setup: full PATH/env, a TERM the TUI renders under, a demo-local FUNES_HOME so the
# remote's disk cache stays off your real ~/.funes, a clean prompt and cwd, and the alias that
# binds `claude` to the funes-only, pre-approved MCP config.
Hide
Type "source ~/.zshrc"
Enter
Type "export TERM=xterm-256color"
Enter
Type "export FUNES_HOME=$PWD/docs/demo"
Enter
Type "export PS1='$ '"
Enter
Type "unset PROMPT_COMMAND"
Enter
Type 'cd "docs/demo"'
Enter
Type 'alias claude="claude --strict-mcp-config --mcp-config funes-mcp.json --allowed-tools mcp__funes__recall mcp__funes__get"'
Enter
Type "clear"
Enter
Show

# claude launches with recall bound to the published memory. Give the funes MCP server a beat to
# connect (model load + remote handshake) before the question — too short and the turn fires
# before the tool is ready.
Type "claude"
Enter
Sleep 5s

# Framed as a past decision so claude recalls rather than reading the source, and counterintuitive
# so a skipped-recall take is visibly wrong: the obvious guess is "yes", funes concluded "no".
Type "Earlier we discussed whether embedding-based semantic expansion improves retrieval quality — what did we conclude? Answer in one sentence, and cite the session."
Enter
Sleep 55s
Ctrl+C
Sleep 500ms
Ctrl+C
Sleep 2s
