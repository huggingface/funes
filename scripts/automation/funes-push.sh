#!/usr/bin/env bash
# Publish the local funes store to a remote store on the HF Hub.
#
# The store to publish to is this script's first argument — `funes add <agent> <store>` bakes it
# into the hook command (`funes-push.sh <org/repo>`). With no store (a local-only install), the
# push hook isn't registered at all, so this script always runs with a store.
#
# Fired by SessionEnd (publish what this session produced) AND SessionStart (catch up
# anything a previous session left unpublished — its SessionEnd may never have fired
# because the host was disconnected, the window closed, or the conversation was
# switched away). Codex has no session-end event, so there it runs on SessionStart only.
# The Stop hook (funes-index.sh) keeps the LOCAL store fresh per turn; this is the only
# step that touches the network, so it runs at session boundaries, not per turn.
#
# `funes push` is incremental and commit-guarded (it retries against a moved remote
# head), so overlapping publishes — and a publish that overlaps an index — are safe;
# no lock is needed. It has a fail-closed secret gate: a chunk holding a credential is
# withheld (exit 2) rather than published. A first push to a store your local store
# shares no chunks with is refused off a terminal — clear it once by hand (see setup).
#
# Runs detached so it never blocks session start/teardown or trips the hook timeout.

set -uo pipefail

# Log beside this script, wherever it was installed. $0 is the script's own path in both modes.
LOG="$(dirname "$0")/funes-sync.log"

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
    local funes rc remote="$1"
    funes="$(find_bin funes || true)"
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "push ABORT: funes not found; skipping."
        return
    fi
    if [ -z "$remote" ]; then
        log "push: skipped (no store bound to this hook)"
        return
    fi

    # A first push to a store this index shares no chunks with is refused here (the overlap
    # guard fails closed off a terminal) — clear it once by hand: `funes push <repo>`.
    log "push: start ($remote)"
    "$funes" push "$remote" >>"$LOG" 2>&1
    rc=$?
    case "$rc" in
        0) log "push: ok" ;;
        2) log "push: WARN — secrets held back; run 'funes scrub', then it publishes next run" ;;
        *) log "push: FAILED (exit $rc)" ;;
    esac
}

# Worker mode (re-exec): do the real work, already detached from the session. The store rides
# through as $2.
if [ "${1:-}" = "--worker" ]; then
    worker "${2:-}"
    exit 0
fi

# Foreground: the store is our first argument. Drain the hook payload on stdin, hand off to a
# detached worker (carrying the store), and return.
REMOTE="${1:-}"
cat >/dev/null
nohup bash "$0" --worker "$REMOTE" >/dev/null 2>&1 </dev/null &
disown 2>/dev/null || true
exit 0
