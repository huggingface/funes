# Demo: tapping a large shared store on the Hub. `funes use <org>/<repo>` points
# this machine's recall at a public HF dataset — here dacorvo/funes-Glint-Research-Fable-5,
# ~21.6k chunks of Fable-5 traces from the "Glint Research" project (a tiny
# terminal coding agent). No local indexing: the store lives on the Hub, and one
# command attaches to it. Then pi — with funes added at user scope — recalls a past
# decision from that corpus, a project this machine never worked on.
#
# The single visible funes command IS the point: memory you don't have to build,
# just point at. Everything else (PATH, TERM, throwaway home) is hidden.
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
# FUNES_HOME so nothing lands in the real ~/.funes, a clean prompt and cwd.
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

# 1) Attach to the shared store. Prints the chunk count — this is a big corpus,
#    and recall reads it straight from the Hub with no local index.
Type "funes use dacorvo/funes-Glint-Research-Fable-5"
Enter
Sleep 4s

# 2) A fresh agent, on a project this machine never touched. pi decides on its own
#    that the answer lives in prior-session memory and reaches for recall.
Type "pi"
Enter
Sleep 3s
Type "I'm picking up the Glint Research project — the tiny terminal coding agent. Before I touch the status bar: we reworked the tokens-per-second counter for accuracy at some point. What window does it average over now, and why did we move off the old one?"
Enter
Sleep 45s
Ctrl+C
Sleep 500ms
Ctrl+C
Sleep 2s
