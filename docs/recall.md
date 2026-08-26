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

The MCP `recall` tool takes the same parameters and defaults, so an agent can widen a search it finds
too narrow — most usefully `half_life: 0` when the answer may be old.

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

## Enumerating with `sessions`

```bash
funes sessions [--repo <owner/name>] [--since <date>] [--until <date>] [--limit <n>] [--offset <n>] [--memory <label>]
```

Recall ranks: it returns the passages closest to a query and says nothing about everything it did
not reach. So it cannot answer *how much is in here*, *which sessions is this about*, or *how much
of it have I looked at*. `sessions` can — it lists the memory's sessions, oldest first, each with
its provenance, turn count, and the prompt it opened with:

```console
$ funes sessions --memory huggingface/funes-memory --since 2026-06-01
[2026-06-19] claude_code huggingface/funes 47 turns 0123456789abcdef
  why did we switch off the streaming parser
…
---
28 sessions
```

That opening prompt is the point. It is the cheapest triage there is — what a session was for,
without reading any of it — and it is the first thing a human actually typed, since injected
scaffolding (`<system-reminder>` wrappers, agent-notes preambles, compaction recaps) is skipped. The
provenance column is the session's source repo when its checkout resolved at index time, and the
working directory when it didn't.

Turns are counted distinct rather than by row, since a long turn is stored as several chunks. The
session id is printed in full, so the line feeds `funes get` or `funes scan` as is.

### Narrowing

Prompts make a row worth reading and also make it longer, so a listing is **bounded to 50 rows by
default**, keeping the most recent, and tells you both what it held back and how to get it:

```console
---
50 of 726 sessions — 676 older: continue with --offset 50, or narrow with --repo/--since/--until
```

The closing line always states the full match count, so coverage stays knowable even when the
listing is partial. `--limit` raises the page to at most 200 rows, and the reply stops at around
40,000 characters regardless — past that it is an answer nobody receives.

To cover the whole population, walk it: `--offset` skips that many of the most recent matches before
taking `--limit`, and the trailer names the offset to use next. (`--limit 0` used to mean "every
match", before a listing had a ceiling; it is now an error rather than an empty reply.) Rows are ordered on (timestamp,
session id), so a given offset always names the same session — a walk neither repeats a row nor skips
one. Date-stepping cannot promise that, since a single day holds many sessions.

Better still, narrow: `--repo owner/name` keeps the sessions whose checkout resolved to that repo
(worktrees and scratch directories of the same clone included, which matching on the working
directory would miss), and `--since`/`--until` bracket the start date inclusively as `YYYY-MM-DD`.

## Finding a literal with `scan`

```bash
funes scan <needle> <session_id> [--from <seq>] [--to <seq>] [--ignore-case] [--context <chars>] [--memory <label>]
```

Recall proves presence. It ranks passages by similarity and returns the best few, so it can show
that a memory discusses something — never that it does not. `scan` is the other half: a literal
string checked against **every** block of one session, so a zero means the string is nowhere in
that session.

```console
$ funes scan "acme-internal" af33dfe0-8576-435d-bc7a-016595b65402 --memory huggingface/funes-memory
scan "acme-internal" in af33dfe0-8576-435d-bc7a-016595b65402 — 2 hits
[2026-07-07T11:34:19.737Z] tool_result seq214
  → get af33dfe0-8576-435d-bc7a-016595b65402 4c1e… --memory huggingface/funes-memory
  … remote add upstream git@github.com:acme-internal/pipeline.git …
```

**A session at a time**, because that is the shape of the question. "Does this session mention an
internal project" is answerable; "is this term anywhere in the memory" was never a clearance for any
particular session, and the verb no longer offers it. Use [`sessions`](#listing-a-memorys-sessions)
to enumerate what to screen, and scan each one you care about — a session read costs a filtered scan
of its own rows, not a pass over the memory.

It is **literal, not a regex** — deliberately. The whole point of the verb is that zero hits reads
as *clean*, and a pattern that silently matches nothing is a false clearance. `--ignore-case` covers
case variation. For the same reason a session id that isn't in the memory is an error rather than an
empty result: a mistyped id must never read as absence.

Splits are stitched back into their block before matching, so a needle spanning a chunk boundary is
still found, and a long block reports one hit rather than one per chunk.

Listing stops at 200 hits, and says where to pick up: `4202 more hits not shown — continue with
--from 3891`. A common word in a long session really does run that far — `"the"` finds 4,402 hits in
a 10,000-turn session — so a cap you could not walk past would turn the count into a dead end. The
resume coordinate is the first hit that was dropped rather than one past the last shown, because a
turn can hold several matching blocks: continuing may repeat a hit, but it never skips one.

`--from`/`--to` take the same `seq` coordinate as [`get`](#reading-turns-with-get), and scan a
stretch of a session on their own — useful for the part you have not screened yet. A window scopes
what a zero means: over `--from 500 --to 999` the reply names those turns and clears only them. A
window that falls outside the session says `no turns in that range` rather than reporting absence.

Absence of a match clears only the literal you passed, in the session you named — pick the spellings
that matter.

## Digesting a session with `sketch`

```bash
funes sketch <session_id> [--units <n>] [--max-chars <n>] [--from <seq>] [--to <seq>] [--memory <label>]
```

Every other read needs you to know what you are looking for: `recall` a query, `scan` a literal,
`get` coordinates, `sessions` the opening prompt and nothing after it. `sketch` asks the session
instead — it selects the passages most *distinctive within it*, using the vectors funes already
stored, with no query at all.

```console
$ funes sketch 987a1e04-98cc-4d18-a618-8efebca34b0d --units 4
sketch 987a1e04-98cc-4d18-a618-8efebca34b0d — 4 of 4 places · 2103 of 8000 chars · 164 eligible units
[2026-08-26T07:27:41.111Z] user text seq0
  → get 987a1e04-98cc-4d18-a618-8efebca34b0d --from 0 --to 3 --memory local
retrieve the personas that were used for the last adversarial review of this blog post
…
---
a sketch samples: it shows what it selected, so it cannot show that anything is absent — scan a literal for that
```

That is the read for an open-ended judgement — *is this session worth publishing, does it reveal
anything internal, what was actually accomplished here.* The selector picks the largest residual off
the session's own mean, so it favours what does not belong: an internal codename inside a session
about refactoring a parser is exactly what it surfaces, and exactly what a summary would drop.

**It is not for reading a session.** Every place carries a `→ get` line, so the surrounding turns are
one call away, verbatim. Reading a session end to end costs more than everything else together.

`--units` sets how many places (default 8, at most 40), `--max-chars` the total budget (default
8,000, at most 40,000). They interact: each place is guaranteed 240 characters, so `--units` is
clamped to what the budget can render — and every clamp is stated in the reply rather than applied
quietly. No single passage may take more than its share, so one turn carrying a file cannot become
the whole digest; a shortened passage says so.

For a long session, digest it in stretches with `--from`/`--to`: eight places over 13,000 turns is
thin, where eight over each 2,000 is proportional. A whole-session digest is cached (it is
deterministic in the session's content), which takes a 13,000-turn sketch from 1.8 s to 0.15 s on a
second pass.

A sketch **samples**. It states what it selected and what it left out, so it can never show that
something is absent — that is [`scan`](#finding-a-literal-with-scan), for the literal you pass. The
pipeline is: sketch to find out what is there, scan to establish how often a term you found occurs,
get to read the evidence.

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
