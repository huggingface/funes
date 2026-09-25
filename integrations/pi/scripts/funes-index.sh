#!/bin/sh
# pi's automation: advance funes's index over this agent's spool and, at a session boundary,
# publish it. The extension converts each turn itself and spawns this script detached, so the work is
# done in place — nothing here waits on a hook.
#
#   funes-index.sh                     per turn: index
#   funes-index.sh --publish [MEMORY]  at a boundary: index, waiting out a per-turn run that still
#                                      holds the memory lock, then push to MEMORY — or to the memory
#                                      `setup add` recorded beside this script
#
# No locking: `funes` serializes local-memory writes itself. A per-turn run that loses the lock
# exits non-zero, is logged, and the next turn re-sweeps the same content — indexing is idempotent.
set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
LOG="$HERE/funes-sync.log"
HARNESS=pi

log() { printf '%s %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)" "$*" >>"$LOG"; }

# Resolve a binary by name, falling back to common install dirs — an agent can run with a minimal
# PATH, e.g. launched from an IDE.
find_bin() {
    command -v "$1" 2>/dev/null && return 0
    for d in "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin "$HOME/go/bin" /usr/bin /bin; do
        [ -x "$d/$1" ] && {
            printf '%s\n' "$d/$1"
            return 0
        }
    done
    return 1
}

index() {
    log "index[$HARNESS]: start"
    if "$funes" index --harness "$HARNESS" >>"$LOG" 2>&1; then
        log "index[$HARNESS]: ok"
    else
        log "index[$HARNESS]: FAILED (exit $?)"
    fi
}

# A first push to a memory this index shares no chunks with is refused off a terminal; `funes add`
# clears it once, interactively.
publish() {
    memory=${1:-}
    [ -n "$memory" ] || memory=$(head -n 1 "$HERE/memory" 2>/dev/null | tr -d '[:space:]')
    for attempt in 1 2 3 4 5; do
        if "$funes" index --harness "$HARNESS" >>"$LOG" 2>&1; then
            log "index[$HARNESS]: ok (before push)"
            break
        fi
        log "index[$HARNESS]: busy or failed, retry $attempt"
        sleep 2
    done
    if [ -z "$memory" ]; then
        log "push: skipped (no memory bound)"
        return
    fi
    log "push: start ($memory)"
    "$funes" push "$memory" >>"$LOG" 2>&1
    rc=$?
    case "$rc" in
    0) log "push: ok" ;;
    2) log "push: WARN — secrets held back; run \`funes scrub\`, then it publishes next run" ;;
    *) log "push: FAILED (exit $rc)" ;;
    esac
}

funes=$(find_bin funes || true)
if [ -z "$funes" ] || [ ! -x "$funes" ]; then
    log "index ABORT: funes not found; skipping."
    exit 0
fi
case "${1:-}" in
--publish) publish "${2:-}" ;;
*) index ;;
esac
