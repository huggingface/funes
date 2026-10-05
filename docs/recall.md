# Recalling

`funes recall "<free text>"` retrieves the passages from your past sessions that answer a question,
with the exact session and turn each came from. `funes get` drills into any hit to read the turns
around it. These are the two tools [`funes add`](add.md) gives your agent — and the model reaches for
them on its own — but they work the same from a terminal.

```bash
funes recall "why did we switch off the streaming parser"
```

Retrieval is one pipeline: hybrid search (vector + BM25, fused by reciprocal rank) → neighbor
expansion, with a cross-encoder rerank of the pool before the cut when the pool is several times
the hits asked for. What comes back is the **actual passage from the actual turn**, not a summary
written about it.

Passages are searchable by their words as soon as their text is indexed, even while embeddings
are pending. If text search fails, recall reports an error.

`funes recall` prints one stable, parseable layout — the **agent format** — everywhere, terminal or
pipe. It's shaped for an agent to read, but it's the raw evidence for you too. If you want an
*answer* rather than ranked passages, [`funes ask`](ask.md) borrows an agent to read the memory and
respond, citing the sessions it drew from. When you can't describe what you're after — you want a
particular session, or every session from a repo or a week — start from
[`funes sessions`](sessions.md) instead.

## Output

Each hit carries its provenance and a ready-to-run drill-down line:

```
[<ts>] <harness> <workdir>/<session8> <block_type>  score=<s.sss>
  → get <session_id> --from <seq> --to <seq> --memory <label>
<the full chunk text>
  ~ [<role> <block_type> seq<N>] <neighbor chunk, first 160 chars>
---
```

The `→ get` line carries exactly the arguments `get` wants, including the memory the hit was read
from — the local one for a turn an agent's bound memory is still owed (see
[add.md](add.md#other-mcp-clients)). `no results` prints when nothing matched. The exact shape is
stable — a contract, not a presentation; don't parse it loosely.
The score is the cross-encoder's probability when the pool was reranked, the fused rank score
otherwise (see `--candidates` below).

## `recall` flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `-k` | 8 | hits returned, each a passage to read |
| `--candidates` | 30 | fused pool the hits are cut from, reranked by the cross-encoder before the cut at four times `-k` or more |
| `--neighbors` | 1 | adjacent chunks (by seq) attached per hit; 0 disables |
| `--type` | — | restrict to `text \| thinking \| tool_use \| tool_result` |
| `--role` | — | restrict to one role, as the turns carry it (an agent's turns say `user \| assistant \| tool \| system`) |
| `--harness` | — | restrict to one harness facet, as the turns carry it (`claude` also matches the older `claude_code`) |
| `--since` / `--until` | — | restrict to turns on or after / on or before a `YYYY-MM-DD` |
| `--memory` | local | the memory to read (see below) |

The MCP `recall` tool takes the same parameters and defaults. Each hit is a passage to read, so
`k` is what a call costs in tokens and `candidates` is how far it looks. Reading more hits is the
cheap way to reach deeper. A pool four times the hits or more is a request to look that deep and
read less: recall then reranks the pool with the cross-encoder before the cut, which costs seconds
per call and grows with the pool. Under that line the hits are the first `k` of the fused order,
and raising `candidates` alone changes little. Ranking weighs relevance, not age: for the latest
word on a topic, pass `since`. Tool output is a `tool_result` block whatever the turn's role, and
in Claude's sessions it arrives on `user` turns, so the user's own words are `--role user --type text`.

## Reading turns with `get`

```bash
funes get <session_id> [--from <seq>] [--to <seq>] [--memory <label>]
```

`get` returns a range of a session's turns, with their splits reassembled into whole blocks. Pass the
same `--memory` the recall hint named, so the drill-down reads the memory the hit came from. The
output is the agent format, the same in a terminal as when piped.

Turns are addressed by `seq`, the session's own dense counter over its turns, so `--from 40 --to 60`
is turns 40 through 60. A hit's `→ get` line hands you a ready-to-run range:

```bash
funes get 987a1e04-… --from 37 --to 43   # as printed by the hit
funes get 987a1e04-… --from 40 --to 60   # move or widen
funes get 987a1e04-…                     # from the start; --from alone reads 20 turns on
```

The turn uuid is provenance, not an address: it identifies a turn across re-indexing, and is printed
with every turn, but nothing takes it as input.

Every read closes with the range it covered and the session's size:

```
---
turns 0-19 of 786
```

A read renders 40,000 characters at most, naming the coordinate to resume from — `9 more turn(s) in
range not shown — read them with --from 12`. A turn renders whole or not at all, so a single turn
larger than that is the one thing that can exceed it. It prints `no turns in that range of session
<id>` when the coordinates land outside the session, and errors with `no session <id> in <label>`
when the id is unknown.

## Reading a different memory

`--memory` takes an `<org>/<repo>` shorthand, a full `hf://…` URI, a local path, or `local`. This is
how you read a **shared** memory without changing your own setup:

```bash
funes recall "why is funes append-only" --memory huggingface/funes-memory
```

Recall over a remote caches whole files to local disk, so warm calls run at local speed — see
[hub-caching.md](hub-caching.md). Publishing your own memory to read this way is covered in
[push.md](push.md).

## See also

- [sessions.md](sessions.md) — list a memory's sessions, digest one with `sketch`, scan one for a literal.
- [ask.md](ask.md) — get a grounded answer instead of ranked passages.
- [push.md](push.md) — publishing a memory, and inspecting one with `status`.
- [hub-caching.md](hub-caching.md) — how recall over a remote caches to local disk.
