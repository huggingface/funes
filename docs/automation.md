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

### 1. Build the local index

```bash
funes index          # parse → chunk → embed ~/.claude/projects → ~/.funes
```

Run this once, interactively — **the hook won't build the first index for you.** An
automated (non-terminal) `funes index` refuses to build a from-scratch index (it can take
a long time) and refuses to run with no target; it only does incremental, per-harness
updates once a local index exists. So this manual first index is required on each machine.
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
store your local index shares no chunks with.** That guard exists so an unattended
push can never silently dump your sessions into the wrong repo. A brand-new store —
or an established shared store seen from **a new machine** whose index is all its
own sessions — shares nothing yet, so a *non-interactive* push (like the one a hook
makes) aborts with:

```
refusing to push N chunk(s) to <store>: your local index shares no chunks with it
(a first push, a new host, or the wrong store) — re-run with `--yes` to confirm.
```

Doing the first push interactively (or with `--yes`) clears it: afterwards your
local index and the remote share chunks, so every later push — including the
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
  network**. `Stop` fires after every turn, so the local index tracks the session as
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

### `~/.claude/hooks/funes-index.sh`

Save this and `chmod +x` it:

```bash
#!/usr/bin/env bash
# Index new Claude Code turns into funes — local only, no network.
#
# Fired by the Stop hook (after every turn), so the local index stays fresh as you
# work. `funes index` is incremental and idempotent — it re-embeds nothing already
# stored — and indexing per turn yields the same chunks as indexing once per session,
# since chunk ids derive from (session, turn, block, split). Publishing to the remote
# is a separate step (funes-push.sh, on session boundaries), so this per-turn run
# stays cheap and never touches the network.
#
# The work runs detached so it never blocks the turn or trips the hook timeout: the
# foreground half drains the hook payload, re-execs as a background worker, and
# returns. Detach and locking use only portable idioms (nohup + a mkdir lock) — stock
# macOS ships neither setsid nor flock, and this runs on macOS and Linux.

set -uo pipefail

LOG="$HOME/.claude/hooks/funes-sync.log"
LOCK="$HOME/.claude/hooks/.funes-index.lock"   # serialize concurrent index runs
STALE_LOCK_SECS=1800                           # take over a lock a hard-killed run left behind

log() { printf '%s %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)" "$*" >>"$LOG"; }

# Resolve a binary by name, falling back to common install dirs — hooks can run with a
# minimal PATH (e.g. launched from the IDE).
find_bin() {
    command -v "$1" 2>/dev/null && return 0
    for d in "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin "$HOME/go/bin" /usr/bin /bin; do
        [ -x "$d/$1" ] && { printf '%s\n' "$d/$1"; return 0; }
    done
    return 1
}

# mtime in epoch seconds — GNU `stat -c` first, BSD `stat -f` second.
lock_mtime() { stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || echo 0; }

worker() {
    local funes holder mtime now stolen
    funes="$(find_bin funes || true)"
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "index ABORT: funes not found; skipping."
        return
    fi

    # Serialize concurrent turns/sessions. mkdir is atomic; steal an existing lock only
    # when its holder is truly gone (PID liveness, not wall-clock age — a long or
    # laptop-slept run must not have its lock stolen while it is still working).
    if ! mkdir "$LOCK" 2>/dev/null; then
        holder="$(cat "$LOCK/pid" 2>/dev/null || true)"
        if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
            log "index: another run (pid $holder) in progress; skipping."
            return
        fi
        if [ -z "$holder" ]; then
            mtime="$(lock_mtime "$LOCK")"; now="$(date +%s)"
            if [ "$mtime" -gt 0 ] && [ "$((now - mtime))" -le "$STALE_LOCK_SECS" ]; then
                log "index: lock being acquired by another worker; skipping."
                return
            fi
        fi
        stolen="$LOCK.stale.$$"
        if mv "$LOCK" "$stolen" 2>/dev/null; then
            rm -rf "$stolen" 2>/dev/null
            log "index: reclaimed abandoned lock (holder=${holder:-unknown})"
        fi
        mkdir "$LOCK" 2>/dev/null || { log "index: another worker grabbed the lock; skipping."; return; }
    fi
    printf '%s\n' "$$" >"$LOCK/pid" 2>/dev/null || true
    trap 'rm -rf "$LOCK" 2>/dev/null' EXIT

    # Name the target explicitly: an automated (no-TTY) `funes index` refuses to run with
    # no target, so this indexes only Claude sessions — other agents get their own hook.
    log "index: start"
    if "$funes" index --harness claude >>"$LOG" 2>&1; then
        log "index: ok"
    else
        log "index: FAILED (exit $?)"
    fi
}

# Worker mode (re-exec): do the real work, already detached from the turn.
if [ "${1:-}" = "--worker" ]; then
    worker
    exit 0
fi

# Foreground: drain the Stop payload on stdin, hand off to a detached worker, return.
cat >/dev/null
nohup bash "$0" --worker >/dev/null 2>&1 </dev/null &
disown 2>/dev/null || true
exit 0
```

### `~/.claude/hooks/funes-push.sh`

Save this and `chmod +x` it:

```bash
#!/usr/bin/env bash
# Publish the local funes index to the active remote.
#
# Fired by SessionEnd (publish what this session produced) AND SessionStart (catch up
# anything a previous session left unpublished — its SessionEnd may never have fired
# because the host was disconnected, the window closed, or the conversation was
# switched away). The Stop hook (funes-index.sh) keeps the LOCAL index fresh per turn;
# this is the only step that touches the network, so it runs at session boundaries,
# not per turn.
#
# `funes push` is incremental and commit-guarded (it retries against a moved remote
# head), so overlapping publishes — and a publish that overlaps an index — are safe;
# no lock is needed. It has a fail-closed secret gate: a chunk holding a credential is
# withheld (exit 2) rather than published. A first push to a store the local index
# shares no chunks with is refused off a terminal — clear it once by hand (see setup).
#
# Runs detached so it never blocks session start/teardown or trips the hook timeout.

set -uo pipefail

LOG="$HOME/.claude/hooks/funes-sync.log"
FUNES_HOME="${FUNES_HOME:-$HOME/.funes}"
FUNES_JSON="$FUNES_HOME/funes.json"

log() { printf '%s %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)" "$*" >>"$LOG"; }

# Resolve a binary by name, falling back to common install dirs — hooks can run with a
# minimal PATH (e.g. launched from the IDE).
find_bin() {
    command -v "$1" 2>/dev/null && return 0
    for d in "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin "$HOME/go/bin" /usr/bin /bin; do
        [ -x "$d/$1" ] && { printf '%s\n' "$d/$1"; return 0; }
    done
    return 1
}

worker() {
    local funes rc remote
    funes="$(find_bin funes || true)"
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "push ABORT: funes not found; skipping."
        return
    fi

    # Push only when a remote is attached, naming it explicitly so the target is
    # unambiguous in the log and immune to the active store changing mid-run. A first
    # push to a store this index shares no chunks with is refused here (the overlap
    # guard fails closed off a terminal) — clear it once by hand: `funes push <repo>`.
    remote="$(sed -n 's/.*"remote"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$FUNES_JSON" 2>/dev/null)"
    if [ -z "$remote" ]; then
        log "push: skipped (no remote attached — 'funes use <org>/<repo>' to enable)"
        return
    fi
    log "push: start ($remote)"
    "$funes" push "$remote" >>"$LOG" 2>&1
    rc=$?
    case "$rc" in
        0) log "push: ok" ;;
        2) log "push: WARN — secrets held back; run 'funes scrub', then it publishes next run" ;;
        *) log "push: FAILED (exit $rc)" ;;
    esac
}

# Worker mode (re-exec): do the real work, already detached from the session.
if [ "${1:-}" = "--worker" ]; then
    worker
    exit 0
fi

# Foreground: drain the hook payload on stdin, hand off to a detached worker, return.
cat >/dev/null
nohup bash "$0" --worker >/dev/null 2>&1 </dev/null &
disown 2>/dev/null || true
exit 0
```

### Register all three hooks in `~/.claude/settings.json`

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-index.sh\"",
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

## Verify it

Take a couple of turns in one session, then start a fresh session and:

```bash
tail ~/.claude/hooks/funes-sync.log   # index: ok (per turn) · push: ok (per boundary)
funes status                          # chunk count grows across turns; publishes at start/end
```

## How it behaves

- **Local-first, always safe.** `funes-index.sh` only ever writes `~/.funes`; only
  `funes-push.sh` touches the network. With no remote attached, the push logs
  `push: skipped` — never an error.
- **The first index and first push are manual.** The index hook only does incremental,
  per-harness updates; the push hook's first publish to a store it shares no chunks
  with is refused off a terminal. So a fresh machine does nothing until you run
  `funes index` and `funes push <repo>` once by hand (setup steps 1 and 3).
- **Fresh every turn.** `Stop` re-indexes after each turn, so the local index tracks
  the session as it grows; a session killed mid-flight is already indexed up to its
  last completed turn. Because `funes index` is incremental, that re-sweep is cheap.
- **Published at both boundaries.** `SessionEnd` publishes what the session produced;
  `SessionStart` republishes anything a prior session left unpushed — the recovery
  path for a `SessionEnd` that never fired (host disconnect, closed window, a
  switched-away conversation).
- **Serialized where it matters.** Concurrent `funes index` runs would race on the
  store, so `funes-index.sh` holds a lock and a contended run just skips — its turns
  are swept by the next turn's run. `funes push` needs no lock: it is commit-guarded
  on the remote and retries on conflict, so overlapping publishes (and a publish that
  overlaps an index) are safe.
- **The remaining gap.** A session's very last turn is published no later than the
  next session's start; a machine retired without ever starting another session keeps
  its last unpushed turns local. If that matters, run `funes push <repo>` by hand
  before stepping away.

## Other agents

The automation *pattern* is agent-agnostic: point equivalent hooks — or a
cron/launchd/systemd timer, if your agent has no session hook — at scripts like the
ones above. Only the *target* is Claude-specific, and an automated run must name one:
`funes index --harness claude | codex | pi` indexes that agent's standard session dir,
or pass an explicit path — `funes index <path>`, a directory of session transcripts or a
trace `.parquet`. Everything downstream — chunk, embed, store, push — is identical.
