# Recalling

`funes recall "<free text>"` retrieves the passages from your past sessions that answer a question,
with the exact session and turn each came from. `funes get` drills into any hit to read the turns
around it. These are the two tools [`funes add`](add.md) gives your agent — and the model reaches for
them on its own — but they work the same from a terminal.

```bash
funes recall "why did we switch off the streaming parser"
```

Retrieval is one pipeline: hybrid search (vector + BM25, fused by reciprocal rank) → cross-encoder
rerank → recency reweight → neighbor expansion. What comes back is the **actual passage from the
actual turn**, not a summary written about it.

`funes recall` prints one stable, parseable layout — the **agent format** — everywhere, terminal or
pipe. It's shaped for an agent to read, but it's the raw evidence for you too. If you want an
*answer* rather than ranked passages, [`funes ask`](ask.md) borrows an agent to read the memory and
respond, citing the sessions it drew from.

## Output

Each hit carries its provenance and a ready-to-run drill-down line:

```
[<ts>] <harness> <workdir>/<session8> <block_type>  score=<s.sss>
  → get <session_id> --from <seq> --to <seq> --memory <label>
<the full chunk text>
  ~ [<role> <block_type> seq<N>] <neighbor chunk, first 160 chars>
---
```

The `→ get` line carries exactly the arguments `get` wants, including the memory the hits were read
from. `no results` prints when nothing matched. The exact shape is a contract — see
[AGENTS.md](../AGENTS.md); don't parse it loosely.

## `recall` flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `-k` | 8 | hits returned |
| `--candidates` | 30 | fused pool reranked before the top-k cut |
| `--half-life` | 30 | recency decay in days (a hit this old keeps half its weight); 0 disables |
| `--neighbors` | 1 | adjacent chunks (by seq) attached per hit; 0 disables |
| `--type` | — | restrict to `text \| thinking \| tool_use \| tool_result` |
| `--harness` | — | restrict to `claude \| codex \| pi \| hermes` |
| `--memory` | local | the memory to read (see below) |

## Reading turns with `get`

```bash
funes get <session_id> [--from <seq>] [--to <seq>] [--memory <label>]
```

`get` returns a range of a session's turns, with their splits reassembled into whole blocks. Pass the
same `--memory` the recall hint named, so the drill-down reads the memory the hit came from. The
output is the agent format, the same in a terminal as when piped.

**Turns are addressed by `seq`** — the session's own dense counter over its turns — so `--from 40
--to 60` is exactly "turns 40 through 60". There is one way to name a turn, and a hit's `→ get` line
hands it to you already formed:

```bash
funes get 987a1e04-… --from 37 --to 43   # printed by the hit; run it as it stands
funes get 987a1e04-… --from 40 --to 60   # or move and widen, by editing two numbers
funes get 987a1e04-…                     # or start at the beginning of a session you just chose
```

That last form matters as much as the first. `sessions` hands you a session id, so without a
coordinate read there is no way to *start* reading a session you picked — which is how you end up
dumping one to a file and paging it by line number. `--from` alone reads 20 turns on.

The turn uuid is provenance, not an address: it identifies a turn across re-indexing, and is printed
with every turn, but nothing takes it as input.

Every read closes with the coordinates it covered and the session's size, so a partial read says
what it is part of and the next range is obvious:

```
---
turns 0-19 of 786
```

A single turn can carry a whole file, so rendering stops at around 40,000 characters and states the
remainder with the coordinate to resume from — `9 more turn(s) in range not shown — read them with
--from 12`. One turn always renders however large, since a read that answers nothing cannot be
narrowed further. It prints `no turns in that range of session <id>` when the coordinates land
outside it.

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

- [ask.md](ask.md) — get a grounded answer instead of ranked passages.
- [AGENTS.md](../AGENTS.md) — the exact agent-format contract for `recall`/`get`.
- [push.md](push.md) — publishing a memory, and inspecting one with `status`.
- [hub-caching.md](hub-caching.md) — how recall over a remote caches to local disk.
