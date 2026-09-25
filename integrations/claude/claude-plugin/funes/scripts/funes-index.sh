#!/bin/sh
# Claude Code's per-turn capture: convert the transcript that just changed into funes's spool, then
# any other changed since the last run, then advance the index over it. Fired by Stop and by
# SubagentStop, whose payloads arrive on stdin — and, as `funes-index.sh --publish`, by the session
# boundaries, where the same conversion runs first and the publish follows it, so the last turn is
# in the spool before it is indexed and pushed.
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
HARNESS=claude

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

# One transcript into the spool, named after the session it belongs to, which is its own file stem.
convert_one() {
    jq=$1
    src=$2
    [ -n "$src" ] && [ -r "$src" ] || return 0
    stem=${src##*/}
    stem=${stem%.jsonl}
    if PATH="$(dirname "$jq"):$PATH" "$HERE/../convert" "$src" "$SPOOL/$stem.funes.jsonl" 2>>"$LOG"; then
        log "convert: $stem"
    else
        log "convert: FAILED for $src"
    fi
}

# The transcripts changed since the last sweep: sessions whose own hook never fired — untrusted,
# timed out, a host that died mid-turn — and the sub-agents a workflow nests under a session, whose
# SubagentStop funes may not see. What the spool holds cannot say which those are (funes drains it),
# so the sweep keeps its own mark: the previous sweep's stamp, or, before there is one, the `spool`
# record `setup add` wrote just before it converted the history. The transcript the payload named
# was converted already and is skipped.
convert_stale() {
    jq=$1
    named=$2
    projects="$HOME/.claude/projects"
    [ -d "$projects" ] || return 0
    mark="$HERE/swept"
    since=$mark
    [ -e "$since" ] || since="$HERE/spool"
    # Stamped before the sweep, so a transcript written while it runs is swept again next turn.
    : >"$mark.new"
    find "$projects" -type f -name '*.jsonl' -newer "$since" 2>/dev/null | while IFS= read -r src; do
        [ "$src" = "$named" ] || convert_one "$jq" "$src"
    done
    mv -f "$mark.new" "$mark"
}

# The boundary's second half: land the turns in the memory, waiting out a per-turn worker that may
# still hold its lock, then push them to the memory `setup add` recorded beside this script. A first
# push to a memory this index shares no chunks with is refused off a terminal; `funes add` clears it
# once, interactively.
publish() {
    for attempt in 1 2 3 4 5; do
        if "$funes" index --harness "$HARNESS" >>"$LOG" 2>&1; then
            log "index[$HARNESS]: ok (before push)"
            break
        fi
        log "index[$HARNESS]: busy or failed, retry $attempt"
        sleep 2
    done
    memory=$(head -n 1 "$HERE/memory" 2>/dev/null | tr -d '[:space:]')
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

worker() {
    payload=$1
    mode=${2:-}
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
        log "convert ABORT: no spool recorded; re-run \`funes add claude\`."
        return
    fi

    # SubagentStop carries the sub-agent's own transcript in its own field; the session's is still
    # `transcript_path`. A reported bug fires the event for internal calls whose file never exists,
    # which `convert_one` skips.
    agent_src=$(printf '%s' "$payload" | "$jq" -r '.agent_transcript_path // empty' 2>/dev/null || true)
    src=$(printf '%s' "$payload" | "$jq" -r '.transcript_path // empty' 2>/dev/null || true)
    named=${agent_src:-$src}
    convert_one "$jq" "$named"
    convert_stale "$jq" "$named"

    if [ "$mode" = --publish ]; then
        publish
        return
    fi

    log "index[$HARNESS]: start"
    if "$funes" index --harness "$HARNESS" >>"$LOG" 2>&1; then
        log "index[$HARNESS]: ok"
    else
        log "index[$HARNESS]: FAILED (exit $?)"
    fi
}

# Worker mode (re-exec): the payload and the mode are the arguments, and the turn is already over.
if [ "${1:-}" = "--worker" ]; then
    worker "${2:-}" "${3:-}"
    exit 0
fi

mode=${1:-}
payload=$(cat)
nohup sh "$0" --worker "$payload" "$mode" >/dev/null 2>&1 </dev/null &
exit 0
