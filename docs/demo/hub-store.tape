# Demo: tapping a large shared memory on the Hub with no funes command at all.
# The prompt names a public HF dataset — dacorvo/funes-Glint-Research-Fable-5,
# ~21.6k chunks of Fable-5 traces from the "Glint Research" project (a tiny
# terminal coding agent) — and pi, with funes added at user scope, passes it as
# the memory on its recall call: a past decision recalled from a project this
# machine never worked on. No local indexing, nothing attached first.
#
# Zero visible funes commands IS the point: memory you don't have to build or
# attach, just name. Everything else (PATH, TERM, throwaway home) is hidden.
#
# Prerequisites on the render box:
#   - pi installed, authenticated to a model, and funes added at USER scope.
#   - network access to the public dataset on the HF Hub.
#
# ⚠ Drives the real pi TUI against a live model, so:
#   - TERM must be set or it won't render under vhs (done in the Hide block).
#   - whether pi reaches for recall is the model's call — re-render for a clean take.
#   - the Sleeps are first-guess turn budgets and the exit needs tuning.
#
# Regenerate from the repo root:
#   vhs docs/demo/hub-store.tape

Output docs/img/hub-store.gif

Set FontSize 16
Set Width 1200
Set Height 800
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

# Hidden setup: full PATH/env, a TERM the TUI will render under, a throwaway
# FUNES_HOME so recall stays off your real ~/.funes; a clean prompt and cwd.
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

Type "pi"
Enter
Sleep 3s
Type "I'm picking up the Glint Research project — the tiny terminal coding agent, whose memories are in dacorvo/funes-Glint-Research-Fable-5. "
Type "We reworked the tokens-per-second counter at some point — what window does it average over now?"
Enter
Sleep 30s
Ctrl+C
Sleep 500ms
Ctrl+C
Sleep 2s
