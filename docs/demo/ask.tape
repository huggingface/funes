# Demo: the flagship. `funes ask` puts a question to a published memory and borrows a coding
# agent to answer it — nothing installed. The prompt names the public `huggingface/funes-memory`
# dataset; funes recalls from it in-process, hands the passages to claude, and claude answers in
# one turn, grounded, naming the sessions it drew from. While it waits, funes plays its wait
# animation (one of drift / emoji / sine, picked at random per run).
#
# Deliberately narrow and large-typed so it stays legible embedded in the README: FontSize 22,
# Width 820 (the other demos are 16 / 1200). The animation band caps at ~48 columns, so it sits
# comfortably inside the frame.
#
# Prerequisites on the render box:
#   - claude installed and authenticated to a model.
#   - network access to the public `huggingface/funes-memory` dataset on the HF Hub.
#   - the `funes` on PATH must be THIS branch's build (`cargo install --path .` or copy the
#     release binary), or the wait animation and `--memory` flag won't be present.
#
# ⚠ Drives a real claude call against a live model, so:
#   - TERM must be set or it won't render under vhs (done in the Hide block).
#   - the answer's exact wording and length vary run to run — re-render for a clean take, and
#     bump Height if a long answer scrolls out of frame.
#   - the Sleep is a first-guess budget for the remote recall + the agent turn; tune it so the
#     GIF ends holding the finished answer, not mid-generation.
#   - which animation plays is a coin flip — re-render to land a particular one.
#
# Regenerate from the repo root:
#   vhs docs/demo/ask.tape

Output docs/img/ask.gif

Set FontSize 16
Set Width 800
Set Height 500
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

# Hidden setup: full PATH/env, a TERM the animation renders under, a demo-local FUNES_HOME so the
# remote's disk cache stays off your real ~/.funes; a clean prompt and cwd.
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

Type 'funes ask claude "why is funes append-only" --memory huggingface/funes-memory'
Enter
Sleep 45s
Sleep 4s
