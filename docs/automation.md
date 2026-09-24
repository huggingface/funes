# Automating funes

`funes index` is incremental and cheap, but you still have to *remember* to run it. `funes add`
wires the indexing — and, with a shared memory, the publishing — into your agent, so every turn is
captured automatically with no manual step. This document explains what `funes add` sets up and how
it behaves; you rarely need to touch any of it by hand.

![Choosing an embedding model in Claude Code, then Codex recalling that decision in a separate session](img/cross-agents.gif)

*Different agents, one memory: Claude picks an embedding model; a session-end hook indexes it on its own; Codex — a separate agent — uses funes to recall the decision. No `funes` command in sight — the hooks do the capturing.*

*Look closely at Codex's hits: some timestamps predate this recording. Those are earlier takes of this very demo — funes had already memorized the rehearsals. An append-only memory has no clean take, so we kept the Droste effect rather than pretend otherwise.*

## What `funes add` sets up

`funes add claude`, `funes add codex`, `funes add pi`, and `funes add hermes` install, beyond the
read tools:

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
idempotent. On an install an older funes made, remove first — [add.md](add.md) says why.

## How it's wired

Indexing per turn produces **exactly the same chunks** as indexing once at the end: a chunk's id
derives from `(session, turn, block, split)`, so a completed turn's chunks are identical no matter
when they're indexed, and `funes index` re-embeds nothing already written. Keeping the network step
(the push) off the per-turn path is what lets indexing run every turn cheaply.

- **Claude Code** has a plugin system, so funes ships a hooks-only plugin (extracted to
  `~/.funes/agents/claude/claude-plugin`) and registers it with `claude plugin marketplace add` +
  `claude plugin install`. Claude's loader activates the plugin's hooks — **funes never edits your
  `settings.json`**. `funes remove claude` removes the plugin, its local marketplace registration,
  the extracted source, and the separate MCP registration.
- **pi** has no hook system: it exposes its lifecycle to extensions instead, so the automation rides
  in the extension that already gives pi the read tools — the same scripts, run from `turn_end` and
  the session-boundary events. Nothing outside `~/.funes/agents/pi` is configured, so
  `funes remove pi` takes the whole install with it.
- **Codex** has a plugin system too, so funes ships one plugin (installed at
  `~/.funes/agents/codex/codex-plugin`) carrying both its hooks and a small skill, and registers it
  with `codex plugin marketplace add` + `codex plugin add`. The skill is what lets Codex recognize
  funes as memory before it loads any of its tools. **funes never edits your `config.toml`** — Codex
  writes its own — and nothing of funes's goes into Codex's own `hooks.json`.
  **Codex runs a hook only once you have trusted it**, so after installing — and again after any
  change to a funes hook — run `/hooks` in Codex and review them; until then it skips them and
  nothing is indexed or published
  ([Codex docs](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks)). An install from before
  the plugin is cleared on sight, its entries in Codex's own `hooks.json` included — hooks of your
  own in that file stay, and the file goes only once nothing is left in it.
- **Hermes** (indexing is **beta**) discovers plugins under its own home, so funes installs one at
  `~/.hermes/plugins/funes/` and has hermes enable it with `hermes plugins enable funes` — hermes
  edits its own `config.yaml`, funes never does. The plugin's lifecycle hooks (`post_llm_call` per
  completed turn, `on_session_start` + `on_session_finalize` with a memory bound) drive the same two
  scripts as every other agent. Plugin hooks aren't shell hooks, so hermes' consent allowlist
  (`~/.hermes/shell-hooks-allowlist.json`) isn't involved. An install from before the plugin declared
  those hooks in your `config.yaml`; funes can't take them out of the file that holds the rest of
  your configuration, so it names them — until you delete them they simply do the plugin's work a
  second time.

`funes remove hermes` disables the plugin and deletes it, revokes the approvals a pre-plugin
install left in the consent allowlist, and removes funes's own hook scripts and their
`funes-sync.log`; other hooks, approvals, and config keys remain. Removing an integration never
deletes the indexed memory or source transcripts.

Every agent drives the same two scripts, installed alongside: `funes-index.sh` (the per-turn
local index) and `funes-push.sh` (the network publish). Each drains the hook payload and re-execs a
detached worker, so the hook returns in well under a second and never blocks the turn or trips a
timeout.

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
- **The boundary publish converts and indexes first.** The per-turn hook detaches, so a session's
  last turn may still be converting when the boundary fires; the boundary hook converts that
  session itself — and any other changed since the hook last ran — then indexes (waiting out the
  per-turn writer's lock) and pushes, rather than leaving that turn to the next session's catch-up.
- **Published at the boundaries you have.** Claude publishes on `SessionEnd` and again on
  `SessionStart` (catching up anything a missed `SessionEnd` left behind — a disconnect, a closed
  window). Hermes publishes on `on_session_finalize` (its true session end) and again on
  `on_session_start` (the same catch-up). pi publishes on `session_shutdown`, and on `session_start`
  only when the process is fresh — its other starts follow a shutdown that just published. Codex
  publishes on the same pair, `SessionEnd` and `SessionStart`; its hooks ride in a plugin, so a Codex
  too old to have plugins stops the install rather than leaving it silently unpublished.
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
