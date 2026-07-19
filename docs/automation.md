# Automating funes

`funes index` is incremental and cheap, but you still have to *remember* to run it. `funes add`
wires the indexing — and, with a shared store, the publishing — into your agent, so every turn is
captured automatically with no manual step. This document explains what `funes add` sets up and how
it behaves; you rarely need to touch any of it by hand.

## What `funes add` sets up

`funes add claude` and `funes add codex` install, beyond the `recall`/`get` tools:

- **Per-turn indexing.** A `Stop` hook runs `funes index` after every turn, so your local store
  tracks the session as it grows — a session killed mid-flight is already indexed up to its last
  completed turn. Each run is time-boxed (text first, ~60s), so a large backlog fills in a bounded
  step per turn instead of one long sweep.
- **Publishing at session boundaries.** Bind a shared store — `funes add claude <org>/<repo>` — and
  `SessionStart`/`SessionEnd` hooks run `funes push` to publish there. Without a store, indexing is
  local-only and nothing is published.

It also performs the one-time bootstrap the hooks are forbidden to do unattended, so nothing is left
to run by hand after it:

- **Builds your first index** (from that agent's sessions) if you don't have one yet — a fast,
  text-first pass that gets recall working in about a minute, after asking. Deeper content and older
  sessions backfill on later turns. The hooks never trigger a first index; `funes add` does, once you
  accept.
- **Does the first push** to a freshly-bound store. The push hook can't: a first publish to a store
  your local store shares no chunks with is refused off a terminal (the wrong-store guard, below),
  so it must be interactive — `funes add` handles it.

Re-run `funes add claude <org>/<repo>` any time to change the store or refresh the setup — it's
idempotent.

## How it's wired

Indexing per turn produces **exactly the same chunks** as indexing once at the end: a chunk's id
derives from `(session, turn, block, split)`, so a completed turn's chunks are identical no matter
when they're indexed, and `funes index` re-embeds nothing already stored. Keeping the network step
(the push) off the per-turn path is what lets indexing run every turn cheaply.

- **Claude Code** has a plugin system, so funes ships a hooks-only plugin (extracted to
  `~/.funes/integrations/claude-plugin`) and registers it with `claude plugin marketplace add` +
  `claude plugin install`. Claude's loader activates the plugin's hooks — **funes never edits your
  `settings.json`**. Remove it with `claude plugin uninstall funes@huggingface`.
- **Codex** has no plugin system, so funes writes its hooks into `~/.codex/hooks.json` — a file
  dedicated to hooks, not your `config.toml`. The merge is append-or-replace keyed by funes's own
  scripts, so any hooks you added yourself are left untouched. Codex has no session-end event, so it
  publishes on `SessionStart` only.

Both agents drive the same two scripts, installed alongside: `funes-index.sh` (the per-turn local
index) and `funes-push.sh` (the network publish). Each drains the hook payload and re-execs a
detached worker, so the hook returns in well under a second and never blocks the turn or trips a
timeout.

## How it behaves

- **Local-first, always safe.** The index hook only ever writes your local store; only the push hook
  touches the network — and with no store bound, there's no push hook at all.
- **Fresh every turn.** `Stop` re-indexes after each turn; because indexing is incremental, the
  re-sweep is cheap.
- **Published at the boundaries you have.** Claude publishes on `SessionEnd` and again on
  `SessionStart` (catching up anything a missed `SessionEnd` left behind — a disconnect, a closed
  window). Codex has no session-end event, so it publishes at `SessionStart` only; its last turns
  publish no sooner than the next Codex session's start.
- **Serialized in the binary.** funes holds an advisory lock while it mutates the local store, so
  only one writer touches it at a time, whatever launched it (a hook, a manual `funes index`, `funes
  scrub`). A run that hits the lock fails loudly and re-sweeps next turn (indexing is idempotent).
  Reads take no lock. `funes push` is commit-guarded on the remote, so overlapping publishes — and a
  publish overlapping an index — are safe.
- **Secrets held back.** funes redacts credentials at index time; on push, a separate always-on gate
  withholds any chunk that still contains one and exits non-zero (code `2`). Run `funes scrub`, then
  the next push publishes it.
- **The card rides along.** A push to a store at the repo root creates the repo's dataset card
  (tagged `funes`) and keeps its stats fresh — in the same commit as the data. A hand-written
  card is never touched.
- **The wrong-store guard.** A first push to a store your local store shares no chunks with (a first
  push, a new host, or the wrong store) is refused off a terminal. `funes add` clears it by doing
  that first push interactively — so on a new host, re-run `funes add <agent> <org>/<repo>` there
  once.
- **The remaining gap.** A session's last turns publish no later than the next session's start; a
  machine retired without starting another session keeps its last unpushed turns local. Run `funes
  push <org>/<repo>` by hand before stepping away if that matters.

## pi, and other agents

**pi** is trace-only: funes ingests pi sessions by parsing their exported traces, and pi has no hook
system — so there's no per-turn automation for it. Index its traces directly with `funes index
<path-or-repo>`.

**hermes** and **opencode** recall from a store, but funes doesn't index them, so `funes add hermes`
/ `funes add opencode` register recall only. Point them at a shared store to read from —
`funes add hermes <org>/<repo>`.
