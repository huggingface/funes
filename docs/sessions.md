# Browsing sessions

[`funes recall`](recall.md) is the entry point when you can *describe* what you're after. When you
can't — you want a particular session, or every session from a repo or a week — start from the
listing instead:

```bash
funes sessions --repo huggingface/funes --since 2026-08-01
```

`sessions` gives you an id. From there, [`funes sketch`](#digesting-a-session-with-sketch) says what
one session contains, [`funes scan`](#finding-a-literal-with-scan) finds a literal you already know
is in it, and [`funes get`](recall.md#reading-turns-with-get) reads the turns themselves.

## Listing sessions

```bash
funes sessions [--repo <owner/name>] [--since <date>] [--until <date>] [--limit <n>] [--offset <n>] [--memory <label>]
```

`sessions` lists what a memory holds, oldest first: date, harness, source repo (or the working
directory when the checkout didn't resolve), turn count, session id, and the prompt the session
opened with. That opening prompt is what each session was *for*, so a listing is usually enough to
pick the one you want without reading any of it.

| flag | default | effect |
|---|---|---|
| `--repo` | — | keep sessions whose checkout resolved to this `owner/name` |
| `--since` / `--until` | — | keep sessions that started on or after / on or before a `YYYY-MM-DD` |
| `--limit` | 50 | rows to list, keeping the most recent; capped at 200 |
| `--offset` | 0 | skip this many of the most recent matches, to walk back in time |

The trailer states the full match count and, when rows are elided, the offset that continues, so a
listing never drops a session silently. A limit of 0 is an error rather than an empty reply.

## Digesting a session with `sketch`

```bash
funes sketch <session_id> [--units <n>] [--max-chars <n>] [--from <seq>] [--to <seq>] [--memory <label>]
```

`sketch` shows what a session contains without being told what to look for: it selects the passages
most *distinctive within it*, from the vectors funes already stored, and always keeps the opening ask
and the last word of the range it digests. That is the read when you have an id and no query — what
this session was about, what was worked on, where it ended up.

```console
$ funes sketch 987a1e04-98cc-4d18-a618-8efebca34b0d --units 4
sketch 987a1e04-98cc-4d18-a618-8efebca34b0d — 4 of 4 places · 2103 of 8000 chars · 164 eligible units
[2026-08-26T07:27:41.111Z] user text seq0
  → get 987a1e04-98cc-4d18-a618-8efebca34b0d --from 0 --to 3 --memory local
retrieve the personas that were used for the last adversarial review of this blog post
…
---
a sketch samples: it shows what it selected, not that anything is absent — scan a literal for that
```

| flag | default | effect |
|---|---|---|
| `--units` | 8 | places to show; clamped to 40, and to what `--max-chars` fits at 240 chars each |
| `--max-chars` | 8000 | characters rendered in total; clamped to 40000 |
| `--from` / `--to` | whole session | digest one stretch of a long session |
| `--memory` | local | the memory to read |

Places come back in reading order, so a digest reads as a timeline rather than a ranking, and each
one carries a `→ get` line — the surrounding turns are one call away, verbatim.

`--units` is a ceiling, not a target: selection stops when nothing left is distinct enough to earn a
place, and near-duplicates share one, so a loop of the same failing command doesn't fill the digest.
No single passage may take more than its share of the character budget either, so one turn carrying a
file cannot become the whole thing; a shortened passage says so, and so does every clamp. A
whole-session digest is cached — it is deterministic in the session's content — which takes a
13,000-turn sketch from 1.8 s to 0.15 s on a second pass. Re-running it returns the same places: to
see more, raise `--units` or digest one stretch at a time with `--from`/`--to`.

A sketch **samples**: it states what it selected and what it left out, so it can never show that
something is absent. Scanning a literal is what does that.

## Finding a literal with `scan`

```bash
funes scan <needle> <session_id> [--from <seq>] [--to <seq>] [-i] [--context <n>] [--memory <label>]
```

`scan` finds a literal in every block of one session, in reading order, each hit carrying the `→ get`
range that reads the turn around it. The needle is a literal, never a regex: a pattern that silently
matched nothing would read as a clean result.

| flag | default | effect |
|---|---|---|
| `--from` / `--to` | whole session | scan only this seq range |
| `-i`, `--ignore-case` | off | match regardless of case |
| `--context` | 100 | characters of surrounding text shown on each side of a match |
| `--memory` | local | the memory to read |

Splits are stitched back together before matching, so a needle that straddles a chunk boundary is
found. A zero — `no matches for "<needle>" in <session_id>` — covers that session and that exact
spelling, and a windowed scan says which stretch it covered. Listing stops at 200 hits, cut at a turn
boundary, and names the `--from` that continues; if one turn holds more matches than that by itself,
the reply names the turn to read instead.

## See also

- [recall.md](recall.md) — searching by description, and reading turns with `get`.
- [AGENTS.md](../AGENTS.md) — the exact agent-format contract for these commands and their MCP tools.
- [push.md](push.md) — publishing a memory, and inspecting one with `status`.
