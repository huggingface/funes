# Automating funes

`funes index` is incremental and cheap to re-run, but you still have to *remember*
to run it. This guide explains how to wire it into each agent workflow so every session
is indexed — and, optionally, published to a shared Hub store — the moment it ends,
with no manual step.

## Before you start

- **funes on your `PATH`.** The hook resolves the binary from `PATH` and a few
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
If you only want local recall, you're done — skip to [the hook](#claude-code).

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

Two things the push can still report, by design, and the hook handles both:

- **Secrets held back.** A separate, always-on gate withholds any chunk containing
  a credential and exits non-zero (code `2`). Run `funes scrub`, then it publishes
  on the next run. This is independent of the overlap guard above.
- **Read-only token.** If your HF token can't write the store, push says so; recall
  can still read it.

## Claude Code

Save this as `~/.claude/hooks/funes-sync.sh` and `chmod +x` it:

```bash
#!/usr/bin/env bash
# Index new Claude Code turns into funes and publish them to the active remote.
#
# Fired by the SessionEnd hook. The heavy work — embedding new turns, then a
# network push — runs detached so it never blocks session teardown or trips the
# hook timeout: the foreground half drains the hook payload, re-execs itself as a
# background worker, and returns. Detach and locking use only portable idioms
# (nohup + a mkdir lock), because stock macOS ships neither setsid nor flock.

set -uo pipefail

LOG="$HOME/.claude/hooks/funes-sync.log"
LOCK="$HOME/.claude/hooks/.funes-sync.lock"
FUNES_HOME="${FUNES_HOME:-$HOME/.funes}"
FUNES_JSON="$FUNES_HOME/funes.json"
STALE_LOCK_SECS=1800

log() { printf '%s %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)" "$*" >>"$LOG"; }

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
    local funes rc holder mtime now stolen remote
    funes="$(find_bin funes || true)"
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "ABORT: funes not found; skipping."
        return
    fi

    # Serialize concurrent session-ends. mkdir is atomic; steal an existing lock only
    # when its holder is truly gone (PID liveness, not wall-clock age — a long or
    # laptop-slept run must not have its lock stolen while it is still working).
    if ! mkdir "$LOCK" 2>/dev/null; then
        holder="$(cat "$LOCK/pid" 2>/dev/null || true)"
        if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
            log "another sync (pid $holder) in progress; skipping."
            return
        fi
        if [ -z "$holder" ]; then
            mtime="$(lock_mtime "$LOCK")"; now="$(date +%s)"
            if [ "$mtime" -gt 0 ] && [ "$((now - mtime))" -le "$STALE_LOCK_SECS" ]; then
                log "lock being acquired by another worker; skipping."
                return
            fi
        fi
        stolen="$LOCK.stale.$$"
        if mv "$LOCK" "$stolen" 2>/dev/null; then
            rm -rf "$stolen" 2>/dev/null
            log "reclaimed abandoned lock (holder=${holder:-unknown})"
        fi
        mkdir "$LOCK" 2>/dev/null || { log "another sync grabbed the lock; skipping."; return; }
    fi
    printf '%s\n' "$$" >"$LOCK/pid" 2>/dev/null || true
    trap 'rm -rf "$LOCK" 2>/dev/null' EXIT

    # Name the target explicitly: an automated (no-TTY) `funes index` refuses to run with no
    # target, so this indexes only Claude sessions — Codex/pi are handled by their own hooks. It
    # also refuses to build a *first* index unattended, hence the manual `funes index` in setup.
    log "index: start"
    if "$funes" index --harness claude >>"$LOG" 2>&1; then
        log "index: ok"
    else
        log "index: FAILED (exit $?)"
        return
    fi

    # Push only when a remote is attached, and name it explicitly so the target is
    # unambiguous. A first push to a store this index shares no chunks with is refused
    # here (the overlap guard fails closed off a terminal) — clear it with the one-time
    # manual push in setup.
    remote="$(sed -n 's/.*"remote"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$FUNES_JSON" 2>/dev/null)"
    if [ -n "$remote" ]; then
        log "push: start ($remote)"
        "$funes" push "$remote" >>"$LOG" 2>&1
        rc=$?
        case "$rc" in
            0) log "push: ok" ;;
            2) log "push: WARN — secrets held back; run 'funes scrub', then it publishes next run" ;;
            *) log "push: FAILED (exit $rc)" ;;
        esac
    else
        log "push: skipped (no remote attached — 'funes use <org>/<repo>' to enable)"
    fi
}

if [ "${1:-}" = "--worker" ]; then
    worker
    exit 0
fi

# Foreground: drain the SessionEnd payload on stdin, hand off to a detached worker,
# and return at once so session teardown is never blocked.
cat >/dev/null
nohup bash "$0" --worker >/dev/null 2>&1 </dev/null &
disown 2>/dev/null || true
exit 0
```

Then register it in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.claude/hooks/funes-sync.sh\"",
            "timeout": 15,
            "statusMessage": "Indexing session into funes memory"
          }
        ]
      }
    ]
  }
}
```

`timeout` can be short: the hook returns in well under a second because the
indexing and push run in the detached worker, not in the hook Claude Code is
waiting on.

## Verify it

End a session, then:

```bash
tail ~/.claude/hooks/funes-sync.log     # index: ok / push: ok
funes status                            # chunk count should grow across sessions
```

## How it behaves

- **Local-first, always safe.** `funes index` only ever writes `~/.funes`. If no
  remote is attached, the hook indexes and logs `push: skipped` — never an error.
- **The first index is manual.** The hook only does incremental, per-harness updates;
  it refuses to build a from-scratch index unattended, so a fresh machine does nothing
  until you run `funes index` once by hand (setup step 1).
- **Fresh the next session, not the current one.** The worker outlives the session
  that spawned it, but a session's *final* turns land as it tears down. They're
  swept up by the next session's run (or the current run if they were already
  flushed). Because `funes index` is incremental, that re-sweep is cheap.
- **Concurrent sessions are serialized.** If two sessions end at once, one indexes
  and the other logs `skipping`; its turns are picked up by the next run.
- **The only real gap:** a session that is the *last activity before a long idle*
  (or the last ever on a retired machine) may go unindexed until some future
  session fires the hook. If that matters, run `funes index && funes push <repo>`
  by hand before stepping away.

## Other agents

The automation *pattern* is agent-agnostic: point an equivalent hook — or a
cron/launchd/systemd timer, if your agent has no session hook — at a script like the
one above. Only the *target* is Claude-specific, and an automated run must name one:
`funes index --harness claude | codex | pi` indexes that agent's standard session dir,
or pass an explicit path — `funes index <path>`, a directory of session transcripts or a
trace `.parquet`. Everything downstream — chunk, embed, store, push — is identical.
