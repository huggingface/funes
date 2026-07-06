#!/usr/bin/env bash
# Index an agent's new turns into funes — local only, no network.
#
# The harness to index is the first argument (`claude`, `codex`, `pi`, …); it defaults
# to `claude`. One script serves every agent — Claude's Stop hook calls it with `claude`,
# Codex's with `codex` — so there's a single implementation to maintain.
#
# Fired per turn (Claude's Stop hook, Codex's Stop hook), so the local store stays fresh
# as you work. `funes index` is incremental and idempotent — it re-embeds nothing already
# stored — and indexing per turn yields the same chunks as indexing once per session,
# since chunk ids derive from (session, turn, block, split). Publishing to the remote
# is a separate step (funes-push.sh, on session boundaries), so this per-turn run
# stays cheap and never touches the network.
#
# No locking here: `funes` itself serializes local-store writes (an advisory lock in the
# binary). If another store operation holds it, this run exits non-zero (logged below) and
# the next turn re-sweeps the same content — indexing is idempotent, so nothing is lost.
# The script only detaches so it never blocks the turn or trips the hook timeout: the
# foreground half drains the hook payload, re-execs as a background worker, and returns.
# Detach uses only portable idioms (nohup) — this runs on macOS and Linux.

set -uo pipefail

LOG="$HOME/.claude/hooks/funes-sync.log"

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
    local funes
    funes="$(find_bin funes || true)"
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "index ABORT: funes not found; skipping."
        return
    fi

    # Name the target explicitly: an automated (no-TTY) `funes index` refuses to run with
    # no target, so this indexes only the named harness's sessions.
    log "index[$HARNESS]: start"
    if "$funes" index --harness "$HARNESS" >>"$LOG" 2>&1; then
        log "index[$HARNESS]: ok"
    else
        log "index[$HARNESS]: FAILED (exit $?)"
    fi
}

# Worker mode (re-exec): do the real work, already detached from the turn.
# Invoked as: `$0 --worker <harness>`.
if [ "${1:-}" = "--worker" ]; then
    HARNESS="${2:-claude}"
    worker
    exit 0
fi

# Foreground: drain the hook payload on stdin, hand off to a detached worker, return.
# The harness is our first argument (default `claude`). A `notify`-style caller that
# appends its own JSON as a trailing argument is fine — we read only $1 here.
HARNESS="${1:-claude}"
cat >/dev/null
nohup bash "$0" --worker "$HARNESS" >/dev/null 2>&1 </dev/null &
disown 2>/dev/null || true
exit 0
