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

`ask` supports **claude** and **codex**, which ground differently:

- **claude** runs with funes mounted as a **session-only MCP server** (`--strict-mcp-config` keeps
  every persistent registration out) and recalls on its own — the same `recall`/`get` tools it would
  get from `funes add`, but scoped to this one invocation.
- **codex** can't run MCP tools headless (its exec-mode tool-approval elicitation is auto-cancelled),
  so funes recalls **in-process** and embeds the passages in the prompt before handing it off.

Either way the child reads no stdin: the grounding is exactly what funes built, nothing piped in.

## Output and arguments

stdout is the agent's **free-text answer** — unlike the read commands, there's nothing stable to
parse. Quote the question (or put `--` before it) when it contains flag-like words.

| Argument | Meaning |
| --- | --- |
| `<question>` | the question to answer (free text) |
| `--memory <label>` | the memory to ground in — `<org>/<repo>`, an `hf://…` URI, a local path, or `local` (default) |

`ask` reuses recall's defaults (`-k 8`, 30 candidates, 30-day half-life, 1 neighbor) and exposes no
tuning of its own; drop to `funes recall` when you want to adjust retrieval.

## Failure modes

`ask` errors **before any agent spawns** on: a memory that can't be read (missing, empty,
unauthorized, no index yet, or unreachable), a missing agent CLI, and — for codex — zero recalled
passages (there'd be nothing to ground on). A non-zero exit from the agent itself fails `ask`, with
the child's exit code reported.

## See also

- [recall.md](recall.md) — the ranked passages behind an answer, and `get` to read a cited turn.
- [add.md](add.md) — wire an agent permanently instead of borrowing it per question.
- [AGENTS.md](../AGENTS.md) — the exact `ask` contract.
