#!/bin/sh
# Codex's per-turn capture: convert the session that just changed into funes's spool, then advance
# the index over it. Fired by the Stop hook, whose payload arrives on stdin.
#
# The foreground half reads that payload and returns, so a turn never waits on the conversion or on
# the embedder; the worker it leaves behind does both. Detaching uses nohup alone, which is portable
# to macOS and Linux.
#
# No locking: `funes` serializes local-memory writes itself. A run that loses the lock exits
# non-zero, is logged, and the next turn re-sweeps the same content — indexing is idempotent.
set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
LOG="$HERE/funes-sync.log"
# Written by `setup add`: the directory funes drains for this agent.
SPOOL=$(cat "$HERE/spool" 2>/dev/null || true)
CODEX_DIR="${CODEX_HOME:-$HOME/.codex}"
HARNESS=codex

log() { printf '%s %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)" "$*" >>"$LOG"; }

# Resolve a binary by name, falling back to common install dirs — a hook can run with a minimal
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

# The rollout the payload names. `transcript_path` is always a key and sometimes null — an ephemeral
# run writes no file at all — so fall back to the rollout whose name carries the session id. Codex
# suffixes a forked thread's file, hence the second pattern.
rollout() {
    jq=$1
    payload=$2
    path=$(printf '%s' "$payload" | "$jq" -r '.transcript_path // empty' 2>/dev/null || true)
    if [ -n "$path" ] && [ -r "$path" ]; then
        printf '%s\n' "$path"
        return 0
    fi
    sid=$(printf '%s' "$payload" | "$jq" -r '.session_id // empty' 2>/dev/null || true)
    [ -n "$sid" ] || return 1
    find "$CODEX_DIR/sessions" \( -name "rollout-*-$sid.jsonl" -o -name "rollout-*-${sid}_*.jsonl" \) \
        2>/dev/null | head -1
}

worker() {
    payload=$1
    funes=$(find_bin funes || true)
    if [ -z "$funes" ] || [ ! -x "$funes" ]; then
        log "index ABORT: funes not found; skipping."
        return
    fi
    jq=$(find_bin jq || true)
    if [ -z "$jq" ] || [ ! -x "$jq" ]; then
        log "convert ABORT: jq not found; the session was not captured."
        return
    fi
    if [ -z "$SPOOL" ]; then
        log "convert ABORT: no spool recorded; re-run \`funes add codex\`."
        return
    fi

    src=$(rollout "$jq" "$payload" || true)
    if [ -n "$src" ]; then
        stem=${src##*/}
        stem=${stem%.jsonl}
        if PATH="$(dirname "$jq"):$PATH" "$HERE/../convert" "$src" "$SPOOL/$stem.funes.jsonl" 2>>"$LOG"; then
            log "convert: $stem"
        else
            log "convert: FAILED for $src"
        fi
    else
        log "convert: the payload named no readable rollout"
    fi

    log "index[$HARNESS]: start"
    if "$funes" index --harness "$HARNESS" >>"$LOG" 2>&1; then
        log "index[$HARNESS]: ok"
    else
        log "index[$HARNESS]: FAILED (exit $?)"
    fi
}

# Worker mode (re-exec): the payload is the argument, and the turn is already over.
if [ "${1:-}" = "--worker" ]; then
    worker "${2:-}"
    exit 0
fi

payload=$(cat)
nohup sh "$0" --worker "$payload" >/dev/null 2>&1 </dev/null &
exit 0
