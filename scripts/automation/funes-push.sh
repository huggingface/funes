#!/usr/bin/env bash
# Publish the local funes index to the active remote.
#
# Fired by SessionEnd (publish what this session produced) AND SessionStart (catch up
# anything a previous session left unpublished — its SessionEnd may never have fired
# because the host was disconnected, the window closed, or the conversation was
# switched away). Codex has no session-end event, so there it runs on SessionStart only.
# The Stop hook (funes-index.sh) keeps the LOCAL index fresh per turn; this is the only
# step that touches the network, so it runs at session boundaries, not per turn.
#
# Harness-agnostic: `funes push` publishes the whole local index, whatever produced it,
# so this script takes no harness argument — one copy serves every agent.
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
