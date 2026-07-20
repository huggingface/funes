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
*answer* rather than ranked passages, [`funes ask`](ask.md) borrows an agent to read the store and
respond, citing the sessions it drew from.

## Output

Each hit carries its provenance and a ready-to-run drill-down line:

```
[<ts>] <harness> <workdir>/<session8> <block_type>  score=<s.sss>
  → get <session_id> <turn_uuid> --store <label>
<the chunk, first 400 chars>
  ~ [<role> <block_type> seq<N>] <neighbor chunk, first 160 chars>
---
```

The `→ get` line carries exactly the arguments `get` wants, including the store the hits were read
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
| `--store` | local | the store to read (see below) |

## Drilling in with `get`

```bash
funes get <session_id> <turn_uuid> [--window 3] [--store <label>] [--highlight <text>] [--format human|agent]
```

`get` returns the named turn plus the turns within the seq window, with splits reassembled into whole
blocks. Pass the same `--store` the recall hint named, so the drill-down reads the store the hit came
from. Unlike `recall`, `get` has a **human** rendering in addition to the agent format — chosen when
both stdin and stdout are terminals, overridable with `--format`. `--highlight` marks text in that
human rendering (whitespace-insensitive; no effect on the agent format). It prints `turn <uuid> not
found in session <id>` when the turn is absent.

## Reading a different store

`--store` takes an `<org>/<repo>` shorthand, a full `hf://…` URI, a local path, or `local`. This is
how you read a **shared** memory without changing your own setup:

```bash
funes recall "why is funes append-only" --store huggingface/funes-memory
```

Recall over a remote caches whole files to local disk, so warm calls run at local speed — see
[hub-caching.md](hub-caching.md). Publishing your own store to read this way is covered in
[push.md](push.md).

## See also

- [ask.md](ask.md) — get a grounded answer instead of ranked passages.
- [AGENTS.md](../AGENTS.md) — the exact agent-format contract for `recall`/`get`.
- [push.md](push.md) — publishing a store, and inspecting one with `status`.
- [hub-caching.md](hub-caching.md) — how recall over a remote caches to local disk.
