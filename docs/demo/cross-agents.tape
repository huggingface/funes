# Demo: memory across agents, no wiring on screen. funes is already installed
# globally (user scope) in Claude Code and Codex, so recall just runs — no
# `funes add`, no per-tool approval. Claude does a piece of work; a turn-end hook
# indexes that session into funes on its own; then Codex — a separate agent —
# recalls what Claude just decided. No `funes` command appears in the demo.
#
# FUNES_HOME points at this repo's docs/demo, so the memory lives here, off your
# real ~/.funes — a public GIF then only ever surfaces demo content. gitignore
# docs/demo/{store,funes.json,.cache} so they aren't committed.
#
# Prerequisites on the render box:
#   - Claude Code + Codex installed, authenticated, and funes added at USER scope.
#   - the turn-end indexing hook enabled (so Claude's last turn lands before Codex).
#
# ⚠ Drives the real TUIs against a live model, so:
#   - TERM must be set or they won't render under vhs (done in the Hide block).
#   - whether Codex reaches for recall is the model's call — re-render for a clean take.
#   - the Sleeps are first-guess turn budgets and the trust `Enter`s / exits need tuning.
#
# Regenerate from the repo root:
#   vhs docs/demo/cross-agents.tape

Output docs/img/cross-agents.gif

Set FontSize 16
Set Width 1200
Set Height 800
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

# Hidden setup: full PATH/env, a TERM the TUIs will render under, the demo's
# FUNES_HOME, a clean prompt, and a throwaway cwd. Opens the GIF on a blank screen.
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
Type "clear"
Enter
Show

# 1) Claude does a piece of work. No funes tool used here — the turn-end hook
#    indexes the session into FUNES_HOME on its own.
Type "claude"
Enter
Sleep 3s
Type "In one short sentence, no preamble: for a local CPU-only English semantic-search CLI in Rust, which embedding model should I use, and why?"
Enter
Sleep 20s
Ctrl+C
Sleep 500ms
Ctrl+C
Sleep 5s

# 2) A different agent, moments later. Give the turn-end hook a beat to finish
#    indexing the Claude session, clear the screen, then Codex recalls it —
#    cross-agent, cross-session, with no funes command in sight.
Hide
Sleep 8s
Type "clear"
Enter
Show
Type "codex"
Enter
Sleep 3s
Type "Earlier in Claude I chose an embedding model for a local Rust semantic-search CLI. In one line: which model, and why?"
Enter
Sleep 60s
Ctrl+C
Sleep 500ms
Ctrl+C
Sleep 2s
