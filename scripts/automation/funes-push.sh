#!/usr/bin/env bash
# Publish the local funes memory to a remote memory on the HF Hub.
#
# The memory to publish to is this script's first argument and the harness to index is the second —
# `funes add <agent> <memory>` bakes both into the hook command (`funes-push.sh <org/repo> <agent>`).
# With no memory (a local-only install), the push hook isn't registered at all, so this script
# always runs with a memory.
#
# Fired by SessionEnd (publish what this session produced) AND SessionStart (catch up
# anything a previous session left unpublished — its SessionEnd may never have fired
# because the host was disconnected, the window closed, or the conversation was
# switched away).
#
# It indexes the harness before publishing. The per-turn index (funes-index.sh) detaches, so at a
# session boundary the last turn may not be stored yet — publishing first would leave it for the
# next session's catch-up, which is the gap the boundary hook exists to close. Indexing here also
# covers a per-turn run that never happened. It is the same incremental, local, idempotent sweep.
#
# `funes push` is incremental and takes a per-remote lock, so a publish that starts while
# another is still running steps aside and the next session start sweeps it. A publish that
# overlaps an index is safe — push only reads the local memory. It has a fail-closed secret
# gate: a chunk holding a credential is
# withheld (exit 2) rather than published. A first push to a memory your local memory
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
    local funes rc attempt remote="$1" harness="${2:-}"
    funes="$(find_bin funes || true)"
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "push ABORT: funes not found; skipping."
        return
    fi
    if [ -z "$remote" ]; then
        log "push: skipped (no memory bound to this hook)"
        return
    fi

    # Land this session's turns before publishing them. An automated `funes index` bails rather
    # than waits when another writer holds the memory lock — typically the per-turn worker still
    # finishing — so retry a few times, then publish whatever is stored.
    if [ -n "$harness" ]; then
        for attempt in 1 2 3 4 5; do
            if "$funes" index --harness "$harness" >>"$LOG" 2>&1; then
                log "index[$harness]: ok (before push)"
                break
            fi
            log "index[$harness]: busy or failed, retry $attempt"
            sleep 2
        done
    fi

    # A first push to a memory this index shares no chunks with is refused here (the overlap
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

# Worker mode (re-exec): do the real work, already detached from the session. The memory and
# harness ride through as $2 and $3.
if [ "${1:-}" = "--worker" ]; then
    worker "${2:-}" "${3:-}"
    exit 0
fi

# Foreground: the memory is our first argument, the harness our second. Drain the hook payload on
# stdin, hand off to a detached worker (carrying both), and return.
REMOTE="${1:-}"
HARNESS="${2:-}"
cat >/dev/null
nohup bash "$0" --worker "$REMOTE" "$HARNESS" >/dev/null 2>&1 </dev/null &
disown 2>/dev/null || true
exit 0
