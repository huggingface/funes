---
name: funes
description: Recall decisions, rationale, and context from the user's past Claude Code sessions. Use when the user refers to earlier work ("what did we decide", "why did we", "last time", "we discussed"), when you lack context the user assumes you already have, or before re-deriving something that may already have been figured out in a prior session. Also recall before asserting the history of anything — that it was never built, was dropped, is out of scope, or was never discussed; a confident claim about a past decision is the cue you're missing the context recall holds.
---

# funes — recall past sessions

`funes` is a local CLI that searches the user's indexed Claude Code history
(hybrid vector + BM25 search → cross-encoder rerank → recency weighting).

## When to use
- The user references prior decisions, discussions, or work not in the current context.
- You are about to re-investigate or re-decide something that may already be settled.
- You need the user's previously stated preferences or the rationale behind a past choice.
- You're about to claim something about the *past*: "this was never built", "we didn't do
  X", "that's out of scope", "I don't think we discussed Y". That phrasing is the trigger —
  recall it before you commit to the judgment, not after.

## How to use
Run a focused, natural-language query:

```
funes recall "why did we choose lancedb over sqlite" --k 6
```

Each result is one line of provenance — `[timestamp] project/session block_type score` —
then a `→ get <session_id> <turn_uuid>` line, a text preview, and a few neighbor chunks for
context. Higher score = more relevant. Results already blend relevance and recency.

The full surface:
- `recall "<query>"` — narrow with `--type text|thinking|tool_use|tool_result` and
  `--project <name>`; tune with `--k N` (results, default 8), `--candidates N` (rerank pool,
  default 30), `--half-life DAYS` (recency decay, default 30; `0` disables),
  `--neighbors N` (adjacent chunks per hit, default 1; `0` disables).
- `list` — browse indexed sessions (`--project <name>`, `--limit N`).
- `get <session_id> <turn_uuid>` — expand a hit into its full surrounding turns
  (`--window N`, default 3).

## Drilling down
Results are ~400-char previews. If a hit is fuzzy or dated, don't settle for it. Either
re-query with a sharper phrase, or use the hit's `→ get <session_id> <turn_uuid>` line to
pull the full surrounding turns (`funes get …`, or the `get` tool) — that's the second half
of the drill-down, not optional. Treat tool-result hits (file reads, command output) as
possibly **stale** — re-verify against the live source rather than trusting the recalled
copy, even if recent.

## Freshness
The index updates only when `funes index` runs; the latest turns of the current session may
not be indexed yet. `funes status` shows the chunk count.
