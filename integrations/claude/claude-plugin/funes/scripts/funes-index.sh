#!/bin/sh
# Claude Code's per-turn capture: convert the transcript that just changed into funes's spool, then
# advance the index over it. Fired by Stop and by SubagentStop, whose payloads arrive on stdin.
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

# A sub-agent writes its own transcript under the session's directory, nested a further level when
# it belongs to a workflow, so the sweep recurses. SubagentStop names the one that just finished and
# is the cheap path; this catches any whose event funes did not see. What the spool holds cannot say
# which those are — funes drains it — so the sweep keeps its own mark, per session, and looks only at
# transcripts written since the last one.
convert_subagents() {
    jq=$1
    parent=$2
    dir="${parent%.jsonl}/subagents"
    [ -d "$dir" ] || return 0
    stem=${parent##*/}
    mark="$HERE/swept/${stem%.jsonl}"
    mkdir -p "$HERE/swept"
    # Stamped before the sweep, so one written while it runs is swept again next turn.
    : >"$mark.new"
    if [ -e "$mark" ]; then
        set -- "$dir" -type f -name '*.jsonl' -newer "$mark"
    else
        set -- "$dir" -type f -name '*.jsonl'
    fi
    find "$@" 2>/dev/null | while IFS= read -r src; do
        convert_one "$jq" "$src"
    done
    mv -f "$mark.new" "$mark"
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
        log "convert ABORT: no spool recorded; re-run \`funes add claude\`."
        return
    fi

    # SubagentStop carries the sub-agent's own transcript in its own field; the session's is still
    # `transcript_path`. A reported bug fires the event for internal calls whose file never exists,
    # which `convert_one` skips.
    agent_src=$(printf '%s' "$payload" | "$jq" -r '.agent_transcript_path // empty' 2>/dev/null || true)
    src=$(printf '%s' "$payload" | "$jq" -r '.transcript_path // empty' 2>/dev/null || true)
    if [ -n "$agent_src" ]; then
        convert_one "$jq" "$agent_src"
    else
        convert_one "$jq" "$src"
        convert_subagents "$jq" "$src"
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
