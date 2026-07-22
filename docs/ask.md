# Asking

`funes ask <agent> "<question>"` borrows a coding agent to answer one question, grounded in a memory —
nothing installed. It's the read-only sibling of [`funes add`](add.md): where `add` wires an agent
permanently, `ask` rents it for a single question and leaves no trace.

```bash
funes ask claude "why is funes append-only"
funes ask codex  "what did we decide about the streaming parser" --memory acme/kb
```

Use it to put a question to a memory *yourself* — [`funes recall`](recall.md) returns ranked
passages, `ask` returns an answer. Every answer names the sessions it drew from, so you can trace it
back with `funes get`.

## The agents

`ask` supports **claude** and **codex**. Both get the same forced grounding: funes recalls
in-process, embeds the returned passages in one prompt, and asks the selected agent for one answer.
The child gets no tools and reads no stdin; registered MCP servers are disabled for this invocation,
so the answer can use only the question and passages funes supplied.

This is deliberately different from [`funes add`](add.md), where a persistent agent decides when to
call `recall` and can try another query or drill in with `get`. A one-shot `ask` is faster and more
predictable, but it cannot recover on its own when the initial retrieval misses.

## Output and arguments

stdout is the agent's **free-text answer** — unlike the read commands, there's nothing stable to
parse. Quote the question (or put `--` before it) when it contains flag-like words.

| Argument | Meaning |
| --- | --- |
| `<question>` | the question to answer (free text) |
| `--memory <label>` | the memory to ground in — `<org>/<repo>`, an `hf://…` URI, a local path, or `local` (default) |

`ask` reuses recall's defaults (`-k 8`, 30 candidates, 30-day half-life, 1 neighbor) and exposes no
tuning of its own; drop to `funes recall` when you want to adjust retrieval.

## Data sent to the agent

Embedding, retrieval, and reranking happen locally, including when the memory itself is hosted on
the Hub. After retrieval, `ask` sends the question and recalled passages to the provider configured
for the selected Claude or Codex CLI. Do not use `ask` with a memory whose passages you would not
send to that provider; use `funes recall` to inspect the evidence locally instead.

## Failure modes

`ask` errors **before any agent spawns** on: a memory that can't be read (missing, empty,
unauthorized, no index yet, or unreachable), a missing agent CLI, and zero recalled passages
(there'd be nothing to ground on). A non-zero exit from the agent itself fails `ask`, with the
child's exit code reported.

## See also

- [recall.md](recall.md) — the ranked passages behind an answer, and `get` to read a cited turn.
- [add.md](add.md) — wire an agent permanently instead of borrowing it per question.
- [AGENTS.md](../AGENTS.md) — the exact `ask` contract.
