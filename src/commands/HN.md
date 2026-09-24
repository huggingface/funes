Show HN: Funes – searchable memory over your past AI coding sessions
https://github.com/huggingface/funes

---

I run dozens of agent sessions a week and every one starts from zero. I re-explain why we ruled out an approach, which pitfalls to expect when debugging this particular environment, what's mission-critical and what's feature creep.

That's annoying on a good day. It bites when I hit the context limit (or run out of credits) halfway through a long session. Compacting, or moving to a fresh session or another agent, costs me twice: I lose things I can't tell I've lost, and I pay for the summary either way.

Everything I needed was already sitting in the transcripts on my disk, but nothing ever read them again.

Funes indexes those transcripts (Claude Code, Codex, Pi and Hermes) into a local Lance database and exposes a recall tool over MCP, so when I refer to earlier work the agent searches its own past instead of asking me for details. It's retrieval, not summarization: there's no memory file to write or keep up to date, the index is just a byproduct of working, and the agent pulls the handful of passages that match rather than loading a whole handoff document.

A memory is a dataset, so it's portable: push one to the Hugging Face Hub and a second machine can recall against it directly.

Write-up: https://huggingface.co/blog/funes
