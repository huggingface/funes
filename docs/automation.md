# Automating funes

`funes index` is incremental and cheap to re-run, but you still have to *remember*
to run it. This guide explains how to wire it into each agent workflow so every turn
is indexed — and, optionally, published to a shared Hub store — automatically, with no
manual step.

## Before you start

- **funes on your `PATH`.** The hooks resolve the binary from `PATH` and a few
  common install dirs (`~/.local/bin`, Homebrew, …), because hooks can run with a
  minimal environment. `funes status` should work in a plain shell.
- **(Optional) a Hub store you own**, if you want to sync across machines or a
  team — an `<org>/<repo>` HF dataset you have *write* access to. Skip this to
  automate purely local indexing.

## One-time setup

### 1. Build your local store

```bash
funes index          # parse → chunk → embed ~/.claude/projects → ~/.funes
```

Run this once, interactively — **the hook won't build your store the first time.** An
automated (non-terminal) `funes index` refuses to build a store from scratch (it can take
a long time) and refuses to run with no target; it only does incremental, per-harness
updates once a local store exists. So this manual first build is required on each machine.
If you only want local recall, you're done — skip to [the hooks](#claude-code).

### 2. Attach a shared store (optional)

```bash
funes use <org>/<repo>     # persists hf://datasets/<org>/<repo> as the active store
```

### 3. Push once, by hand — this is the important part

Before automating the push, publish once manually:

```bash
funes push <org>/<repo>    # name the store explicitly
```

Why this can't be skipped: `funes push` **refuses, off a terminal, to publish to a
store your local store shares no chunks with.** That guard exists so an unattended
push can never silently dump your sessions into the wrong repo. A brand-new store —
or an established shared store seen from **a new machine** whose store is all its
own sessions — shares nothing yet, so a *non-interactive* push (like the one a hook
makes) aborts with:

```
refusing to push N chunk(s) to <store>: your local store shares no chunks with it
(a first push, a new host, or the wrong store) — re-run with `--yes` to confirm.
```

Doing the first push interactively (or with `--yes`) clears it: afterwards your
local store and the remote share chunks, so every later push — including the
hook's — has overlap and goes through without prompting. **Run this one-time push
on each machine you set up.**

> `--yes` also works in the hook itself if you truly want zero manual steps on new
> hosts — at the cost of the wrong-store guard. Prefer the one-time manual push;
> reach for `--yes` only for fully unattended provisioning you trust.

Two things the push can still report, by design, and the push hook handles both:

- **Secrets held back.** A separate, always-on gate withholds any chunk containing
  a credential and exits non-zero (code `2`). Run `funes scrub`, then it publishes
  on the next run. This is independent of the overlap guard above.
- **Read-only token.** If your HF token can't write the store, push says so; recall
  can still read it.

## Claude Code

Two scripts split the work along the two operations funes performs — a local index
update and a network publish — and wire each to the hook that fits it:

- **`Stop` → `funes-index.sh`** — index the session's new turns **locally, no
  network**. `Stop` fires after every turn, so the local store tracks the session as
  it grows; a session killed mid-flight (host disconnect, closed window, a
  switched-away conversation) is already indexed up to its last completed turn.
- **`SessionEnd` → `funes-push.sh`** — **publish** what the session produced.
- **`SessionStart` → `funes-push.sh`** — **publish again on the way in**, to catch up
  anything a previous session left unpublished when its `SessionEnd` never fired.

Keeping the network step off the per-turn path is why indexing can run every turn
cheaply. And indexing per turn produces **exactly the same chunks** as indexing once
at the end: a chunk's id derives from `(session, turn, block, split)`, so a completed
turn's chunks are identical no matter when they're indexed, and `funes index`
re-embeds nothing already stored.

### Install the two scripts

Both live in this repo under [`scripts/automation/`](../scripts/automation):
**`funes-index.sh`** (the per-turn local index) and **`funes-push.sh`** (the network
publish). Neither is Claude-specific — `funes-index.sh` takes the harness as its first
argument and `funes-push.sh` publishes the whole store — so Claude and Codex point at one
copy. Install them where the hooks below expect them:

```bash
mkdir -p ~/.claude/hooks
cp scripts/automation/funes-index.sh scripts/automation/funes-push.sh ~/.claude/hooks/
chmod +x ~/.claude/hooks/funes-index.sh ~/.claude/hooks/funes-push.sh
```

Read them before installing — they run on every turn and session boundary. Both are
built the same way: the foreground half drains the hook payload on stdin and re-execs a
**detached worker**, so the hook returns in well under a second and never blocks the turn
or trips the timeout. The worker then:

- **`funes-index.sh`** — runs `funes index --harness <arg>`. Local only, no network. No
  locking in the script: `funes` serializes local-store writes itself (an advisory lock
  in the binary). If another store operation holds the lock, the run exits non-zero
  (logged) and the next turn re-sweeps the same content — indexing is idempotent.
- **`funes-push.sh`** — reads the attached remote from `~/.funes/funes.json` and runs
  `funes push`. Logs a warning, not a failure, if the fail-closed secret gate held
  chunks back (exit 2 → run `funes scrub`).

Everything downstream is identical for every agent; only the harness argument and which
lifecycle events fire the scripts differ.

### Register all three hooks in `~/.claude/settings.json`

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-index.sh\" claude",
            "timeout": 15,
            "statusMessage": "Indexing turn into funes memory"
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-push.sh\"",
            "timeout": 15,
            "statusMessage": "Publishing funes memory"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-push.sh\"",
            "timeout": 15,
            "statusMessage": "Publishing funes memory"
          }
        ]
      }
    ]
  }
}
```

`timeout` can be short: both hooks return in well under a second because the indexing
and the network push run in the detached worker, not in the hook Claude Code is
waiting on.

## Codex

Codex's [hooks](https://developers.openai.com/codex/hooks) give the same two triggers
Claude's do — a per-turn `Stop` and a `SessionStart` — so **the same two scripts drive
it**, no Codex-specific copies needed:

- **`Stop` → `funes-index.sh codex`** — index the turn's new content locally, exactly
  as Claude's `Stop` does. The harness argument is the only difference; the script and
  its log are shared — one implementation, one timeline. A Codex index and a Claude one
  can safely run at once: `funes` serializes local-store writes in the binary, so no
  coordination between the scripts is needed.
- **`SessionStart` → `funes-push.sh`** — publish on the way in. The push script is
  **harness-agnostic** — `funes push` publishes the whole local store, whatever produced
  it — so it needs no `codex` argument and is reused verbatim.

The one structural difference from Claude: **Codex has no session-end event.** So the
`SessionEnd` publish has no equivalent here; publishing happens only on the way in at the
next `SessionStart`. See [the gap](#the-codex-gap) below.

### Register the hooks in `~/.codex/config.toml`

Codex discovers hooks either as inline `[hooks]` tables in `config.toml` or in a
separate `~/.codex/hooks.json`. The TOML form, alongside your other Codex config:

```toml
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = 'bash "$HOME/.claude/hooks/funes-index.sh" codex'
timeout = 15
statusMessage = "Indexing turn into funes memory"

[[hooks.SessionStart]]
[[hooks.SessionStart.hooks]]
type = "command"
command = 'bash "$HOME/.claude/hooks/funes-push.sh"'
timeout = 15
statusMessage = "Publishing funes memory"
```

Or the equivalent `~/.codex/hooks.json`:

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-index.sh\" codex",
            "timeout": 15,
            "statusMessage": "Indexing turn into funes memory"
          }
        ]
      }
    ],
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-push.sh\"",
            "timeout": 15,
            "statusMessage": "Publishing funes memory"
          }
        ]
      }
    ]
  }
}
```

The commands point at the scripts you installed under `~/.claude/hooks/` for the Claude
setup — that path is not Claude-specific, and pointing both agents at one copy keeps a
single implementation to maintain. If you run Codex without Claude, install the two
scripts wherever you like (e.g. `~/.codex/hooks/`) and adjust the paths.

> **Older Codex without the hooks system?** Use the legacy `notify` program for the
> per-turn index — it fires on `agent-turn-complete`, the same moment as `Stop`:
> `notify = ["bash", "/home/you/.claude/hooks/funes-index.sh", "codex"]` (a root key, so
> it must sit above any `[table]` header). `notify` is exec'd directly, not through a
> shell, so use a literal absolute path — `$HOME`/`~` won't expand. It appends its JSON
> payload as a trailing argument, which the script ignores. But `notify` has no
> `SessionStart` equivalent, so you get local indexing only — publish on a timer (see
> [Other agents](#other-agents)) or by hand.

<h3 id="the-codex-gap">The Codex publish gap</h3>

With no session-end event, a Codex session's last turns publish no sooner than the
**next** Codex `SessionStart`; a machine retired without starting another Codex session
keeps them local until you run `funes push <repo>` by hand. Two things narrow this:

- **`SessionStart` fires on `resume`, `clear`, and `compact`**, not just fresh startup,
  so long-lived work publishes at each of those points, not only when you next launch.
- **If you also run Claude on the same machine**, its `SessionEnd`/`SessionStart` pushes
  publish the Codex-indexed turns too — `funes push` is whole-store — so any agent's
  push boundary flushes every agent's freshly indexed turns.

## Verify it

Take a couple of turns in one session, then start a fresh session and:

```bash
tail ~/.claude/hooks/funes-sync.log   # index[<harness>]: ok (per turn) · push: ok (per boundary)
funes status                          # chunk count grows across turns; publishes at start/end
```

Both agents log to the same `funes-sync.log`, tagged by harness (`index[claude]`,
`index[codex]`), so it's one timeline for everything.

## How it behaves

- **Local-first, always safe.** `funes-index.sh` only ever writes `~/.funes`; only
  `funes-push.sh` touches the network. With no remote attached, the push logs
  `push: skipped` — never an error.
- **The first index and first push are manual.** The index hook only does incremental,
  per-harness updates; the push hook's first publish to a store it shares no chunks
  with is refused off a terminal. So a fresh machine does nothing until you run
  `funes index` and `funes push <repo>` once by hand (setup steps 1 and 3).
- **Fresh every turn.** `Stop` re-indexes after each turn, so the local store tracks
  the session as it grows; a session killed mid-flight is already indexed up to its
  last completed turn. Because `funes index` is incremental, that re-sweep is cheap.
- **Published at the boundaries you have.** On Claude, `SessionEnd` publishes what the
  session produced and `SessionStart` republishes anything a prior session left unpushed
  — the recovery path for a `SessionEnd` that never fired (host disconnect, closed
  window, a switched-away conversation). Codex has no session-end event, so it publishes
  only at `SessionStart`; see [the Codex gap](#the-codex-gap).
- **Serialized in the binary.** `funes` holds an advisory lock while it mutates the local
  store, so only one writer touches it at a time — no matter what launched them (a hook, a
  manual `funes index`, `funes scrub`, a cron timer). A run that hits the lock fails loudly
  rather than waiting or skipping: a per-turn index just re-sweeps next turn (indexing is
  idempotent), and a manual `funes index`/`funes scrub` prints "another funes store
  operation is in progress" so you retry. This is what makes concurrent automation safe, and
  it lives in the binary because the "one writer at a time" rule is a property of the store,
  not of any one script. Reads (recall/get/status) take no lock — they read a consistent
  snapshot. `funes push` targets the remote and is commit-guarded there, so overlapping
  publishes (and a publish that overlaps an index) are safe.
- **The remaining gap.** A session's very last turn is published no later than the
  next session's start; a machine retired without ever starting another session keeps
  its last unpushed turns local. If that matters, run `funes push <repo>` by hand
  before stepping away.

## Other agents

The two scripts above are already agent-agnostic — `funes-index.sh` takes the harness as
its argument and `funes-push.sh` publishes the whole store — so extending to a third
agent (pi, …) is just wiring, not new code. Point that agent's per-turn and per-session
hooks at `funes-index.sh <harness>` and `funes-push.sh`, exactly as Claude and Codex do.

If an agent has no hooks, drive `funes-index.sh <harness>` — or the `funes` binary
directly — from a cron/launchd/systemd timer instead. Either is safe: `funes` serializes
local-store writes in the binary, so a timer firing while a hook run is in flight just
fails loudly and re-sweeps on its next tick. An automated run must always name a target:
`funes index --harness claude|codex|pi` indexes that agent's standard session dir, or pass an explicit path — `funes index <path>`, a directory of transcripts or a trace `.parquet`.
Everything downstream — chunk, embed, store, push — is identical.
