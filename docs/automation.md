# Automating funes

`funes index` is incremental and cheap, but you still have to *remember* to run it. `funes add`
wires the indexing — and, with a shared memory, the publishing — into your agent, so every turn is
captured automatically with no manual step. This document explains what `funes add` sets up and how
it behaves; you rarely need to touch any of it by hand.

![Choosing an embedding model in Claude Code, then Codex recalling that decision in a separate session](img/cross-agents.gif)

*Different agents, one memory: Claude picks an embedding model; a session-end hook indexes it on its own; Codex — a separate agent — uses funes to recall the decision. No `funes` command in sight — the hooks do the capturing.*

*Look closely at Codex's hits: some timestamps predate this recording. Those are earlier takes of this very demo — funes had already memorized the rehearsals. An append-only memory has no clean take, so we kept the Droste effect rather than pretend otherwise.*

## What `funes add` sets up

`funes add <agent>` installs, beyond the read tools:

- **Per-turn indexing.** A per-turn hook converts the session that just changed and runs
  `funes index` after every completed turn, so your local memory tracks the session as it grows — a
  session killed mid-flight is already indexed up to its last completed turn. The same hook converts
  any other session changed since it last ran, so one whose own hook never fired — untrusted, timed
  out, a host that died mid-turn — is captured at the next turn. Each run is time-boxed (text first,
  ~60s), so a large backlog fills in a bounded step per turn instead of one long sweep.
- **Publishing at session boundaries.** Bind a shared memory — `funes add <agent> <org>/<repo>` — and
  session-boundary hooks run `funes push` to publish there. Without a memory, indexing is local-only
  and nothing is published.

It also performs the one-time bootstrap steps, so nothing is left to run by hand after it:

- **Builds your first index** (from that agent's sessions) if you don't have one yet — a fast,
  text-first pass that gets recall working in about a minute, after asking. Deeper content and older
  sessions backfill on later turns. The hooks alone would also fill a cold memory, one bounded step
  per turn; `funes add` builds the most valuable part upfront so recall works from your first
  session.
- **Does the first push** to a freshly-bound memory. The push hook can't: a first publish to a memory
  your local memory shares no chunks with is refused off a terminal (the wrong-memory guard, below),
  so it must be interactive — `funes add` handles it.

Re-run `funes add <agent> <org>/<repo>` any time to change the memory or refresh the setup — it's
idempotent, and it brings an install an older funes made forward too; [add.md](add.md) says what
it clears and when to remove first.

## How it's wired

Indexing per turn produces **exactly the same chunks** as indexing once at the end: a chunk's id
derives from `(session, turn, block, split)`, so a completed turn's chunks are identical no matter
when they're indexed, and `funes index` re-embeds nothing already written. Keeping the network step
(the push) off the per-turn path is what lets indexing run every turn cheaply.

How an integration is wired into its agent — what it installs, which of the agent's events it
hooks, what it needs on the box, and what a re-run clears — is the integration's own to describe.
The maintained ones do so in their READMEs, [`claude`](https://github.com/huggingface/funes-integrations/tree/main/claude),
[`codex`](https://github.com/huggingface/funes-integrations/tree/main/codex),
[`hermes`](https://github.com/huggingface/funes-integrations/tree/main/hermes) and
[`pi`](https://github.com/huggingface/funes-integrations/tree/main/pi), and what they share in
[the repository's README](https://github.com/huggingface/funes-integrations#how-the-bundles-automate).

## Other agents

An agent funes has no parser for joins through the same shape: its per-turn hook runs a converter
that writes the session as a [`.funes.jsonl` turns file](funes-jsonl.md), then `funes index <that
file>` — or, as an installed [integration](add.md#the-integration-contract), into the agent's spool
followed by `funes index --harness <id>`. Three things to know when writing one: an explicit path is indexed in full, unbudgeted — a
single session is small, so that is what you want; a run that finds the memory lock busy fails fast,
and the next turn's run catches up (indexing is idempotent); and `funes index --check <file>`
validates a producer's output without writing anything, so run it before wiring the hook.

## How it behaves

- **Local-first, always safe.** The index hook only ever writes your local memory; only the push hook
  touches the network — and with no memory bound, there's no push hook at all.
- **Fresh every turn.** Each completed turn re-indexes; because indexing is incremental, the
  re-sweep is cheap.
- **Published at the boundaries.** With a memory bound, an integration publishes at its agent's
  session end and again at the next session start, catching up anything a missed end left behind —
  a disconnect, a closed window. Which events those are for each agent is in its README.
- **Serialized in the binary.** funes holds an advisory lock while it mutates the local memory, so
  only one writer touches it at a time, whatever launched it (a hook, a manual `funes index`, `funes
  scrub`). A run that hits the lock fails loudly and re-sweeps next turn (indexing is idempotent).
  Reads take no lock. `funes push` takes one of its own, per remote memory — it only reads the local
  memory, so it never blocks an index — and a publish that finds another in flight steps aside; the
  next session start sweeps it. Nothing is serialized across machines: two hosts publishing to one
  memory still race on the Hub.
- **Secrets held back.** funes redacts credentials at index time; on push, a separate always-on gate
  withholds any chunk that still contains one — the clean rows publish, and the push exits non-zero
  (code `2`) only if that leaves nothing to publish. Run `funes scrub`, then the next push includes it.
- **The card rides along.** A push to a memory at the repo root creates the repo's dataset card
  (tagged `funes`) and keeps its stats fresh — in the same commit as the data. A hand-written
  card is never touched.
- **The wrong-memory guard.** A first push to a memory your local memory shares no chunks with (a first
  push, a new host, or the wrong memory) is refused off a terminal. `funes add` clears it by doing
  that first push interactively — so on a new host, re-run `funes add <agent> <org>/<repo>` there
  once, as soon as that host has an index of its own to push.
- **The remaining gap.** A session's last turns publish no later than the next session's start; a
  machine retired without starting another session keeps its last unpushed turns local. Run `funes
  push <org>/<repo>` by hand before stepping away if that matters.
