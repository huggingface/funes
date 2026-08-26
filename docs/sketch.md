# `sketch` — what a session contains, without asking

Status: design proposal — parameters and safeguards

Scope: a new read verb and MCP tool over the existing `session_sketch` selector; no change to the
selector's mathematics

## Why it is a verb

Every other read needs you to know what you are looking for. `recall` needs a query, `scan` needs a
literal, `get` needs coordinates, `sessions` gives you the opening prompt and nothing after it.
Nothing answers *what is in here*.

`sketch` does, and it is the only read that uses the index without a query. The selector is pivoted
Gram-Schmidt over a session's stored vectors: it centres each passage on the session's own mean,
deflates against what it has already chosen, and takes the largest residual. It selects what is most
*unlike* both the session's average and everything picked so far.

That is the right instrument for judging a session against criteria you cannot turn into queries.
The material that disqualifies a session — an internal codename, a customer, a private negotiation,
inside a session otherwise about refactoring a parser — **is** the material that does not belong in
it. A centroid summary drops exactly that; a novelty selector surfaces it.

It samples, so it can never establish absence. The pipeline is **sketch to find out what is there →
`scan` to establish the extent of a term it surfaced → `get` to read the evidence verbatim.**

## Parameters

Five, and the case for each is that an agent has a basis for choosing it. Everything an agent would
be guessing at stays inside the selector — see [what is deliberately not a
parameter](#what-is-deliberately-not-a-parameter).

| parameter | default | range | what it is for |
|---|---|---|---|
| `session_id` | — | required | the session to digest. One per call |
| `units` | 8 | 1–40 | how many distinct places to show — breadth |
| `max_chars` | 8,000 | 1,000–40,000 | total rendered characters — cost |
| `from` / `to` | whole session | any seq | digest a stretch instead of the whole session |
| `memory` | local / server's | — | as every other read verb |

**`units` against `max_chars`.** They trade off, and the trade is real: screening for a mention
wants many short glimpses, understanding a session wants fewer long ones. They are not independent —
40 units inside 1,000 characters is 25 characters each, which is noise. So each unit is guaranteed a
floor of **240 characters**, and `units` is clamped to `max_chars / 240` when it exceeds that. The
clamp is reported, never silent.

**`from` / `to`** use the same coordinate as `get` and the same `seq` a `→ get` line prints. They
matter for long sessions: 8 units over 10,000 turns is thin, so an agent working a large session
digests it in windows — `--from 0 --to 2500`, then `2500`, and so on — and gets coverage proportional
to the session rather than a fixed sample of it.

## Safeguards

The previous curation runs did not overflow because a parameter was wrong. They overflowed because
one call asked for a whole session: run 3 produced a 664 KB dump from a single `get --window 1000`,
and one of its children then spent 39 of its 59 shell calls paging that dump by line number. The
safeguards below are written against that, not against arithmetic mistakes.

The architectural rule: **clamp the output, never validate parameters against each other.** No
combination of arguments can produce a reply past the ceiling, so there is no combination to get
wrong.

1. **One hard ceiling for the read surface.** `SKETCH_HARD_MAX = 40,000` characters — the same
   ceiling `get` already renders under. `max_chars` is clamped to it; rendering stops at it whatever
   selection produced. The selector's current `4_000..=200_000` validation is far too generous for an
   agent-facing call: 200,000 characters is roughly 50,000 tokens in one reply.
2. **A per-unit cap, derived.** No single item renders more than `max_chars / units`. A sketch is
   explicitly a sample, so truncating an item is legitimate and is marked — unlike `get`, where one
   turn must render whole because it is a verbatim read. Without this, one 200 KB tool result eats
   the entire budget and the digest becomes a single blob.
3. **Tool results additionally previewed** at `min(per-unit cap, 2,000)` characters. They are the
   bulk of a session and the least dense per character, but they cannot be excluded: rule-3 evidence
   turns up in them (`?? ../acme-internal/README.md` is a `tool_result`).
4. **No context neighbours.** The picker's version renders unselected passages around each selected
   one, and its own options say they "do not count against `budget`" — an uncounted leak. They are
   dropped: every item carries a `→ get <session_id> --from <seq> --to <seq>` line, so reading around
   an item is `get`'s job, addressed the way everything else is.
5. **One session per call.** No batching, for the reason `scan` takes one session: a batched reply is
   a blob whose per-session boundaries the caller has to re-derive, and N sessions multiply the
   ceiling by N.
6. **Every reply states what it left out.** Units rendered of units selected, characters of budget,
   and the turn range covered of the session's total. An agent that believes it saw everything is the
   failure this whole design is about.
7. **Clamps are reported, not silently applied.** Asking for 100,000 characters returns 40,000 *and*
   a line saying so. Same for a clamped `units`.
8. **An unknown session is an error**, as in `scan` — a mistyped id must not read as "this session
   holds nothing". A range that selects nothing says so, with the session's size.

## Rendering

Agent format, consistent with the rest of the read surface — a header, a runnable `→ get` line, the
passage:

```
sketch <session_id> — <n> of <m> units · <chars> of <budget> chars · turns <a>-<b> of <total>
[<ts>] <role> <block_type> seq<N>
  → get <session_id> --from <a> --to <b> --memory <label>
  <the passage, truncated at the per-unit cap and marked when it is>
…
---
<k> unit(s) selected but not rendered — raise --max-chars, or digest a narrower --from/--to
```

Deterministic: the same arguments over the same session return the same digest, so a re-call is
free and comparable. The cache is keyed on the session's content and the embedding model, and is
invisible to the caller.

## The tool description

This is the text an agent reads before choosing, so it has to name the attractor it is competing
with. Agents in all three curation runs reached for "read the whole session" first.

> A query-independent digest of one session: the passages most distinctive *within* it, chosen from
> the stored embeddings with no query at all. This is the only read that tells you what a session
> contains when you do not already know what you are looking for — `recall` needs a query, `scan`
> needs a literal, `get` needs coordinates.
>
> Use it to judge a session against open-ended criteria — is this worth publishing, does it reveal
> anything internal, what was actually accomplished here. The material that disqualifies a session is
> usually the material that does not belong in it, which is what a distinctiveness selector surfaces
> and a summary drops.
>
> **Do not read the whole session instead.** Sessions run to thousands of turns; `get`-ing one end to
> end costs more than every other call together, overflows the reply, and leaves you paging a
> transcript by line number. A sketch is a handful of places and a few thousand characters, and every
> one carries a `→ get` line for when you want the surrounding turns verbatim. Sketch to find out
> what is there, `scan` to establish how often a term you found actually occurs, `get` to read the
> evidence.
>
> A sketch samples. It states what it selected and what it left out, so it can never show that
> something is *absent* — that is `scan`, and only for the literal you pass.

## What is deliberately not a parameter

- **The near-duplicate threshold, the LSH bands, the quality weights, the axis/transition split.**
  Selector internals. An agent has no basis for choosing them, and exposing a number invites
  cargo-culting it.
- **`role` / `block_type` filters.** A filtered digest answers a different question, and the
  user-text-only projection is the one already rejected in favour of this verb. Excluding tool results
  would be actively wrong (safeguard 3).
- **`neighbors`.** `get` reads around a coordinate; every item hands one over.
- **A batch of sessions.** Safeguard 5.
- **Output format toggles.** One format, as everywhere else in the read surface.
