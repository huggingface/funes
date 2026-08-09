#!/usr/bin/env bash
# Handover-vs-recall — one engine. Experiments are DATA (bundles on the dataset), not code here.
#
#   ./run.sh <experiment> <A|B|C|D> [rep]
#
# It fetches the experiment bundle artifacts/<experiment>/ from the dataset (config.sh, task.md,
# grader.rs, docs/, fork.jsonl, memory/) and runs one arm against it. See README.md for the
# bundle contract — how to author a new experiment. Arms:
#   A branch-only   fresh, funes OFF                          -> re-derives
#   B handoff       fresh, reads the frozen handoff in docs/  -> consumes the summary
#   C recall        fresh, recalls a LOCAL copy of the memory -> recovers the decision
#   D same-session  resume the fork (full context)
set -euo pipefail

EXP_NAME="${1:?usage: run.sh <experiment> <A|B|C|D> [rep]}"
ARM="${2:?usage: run.sh <experiment> <A|B|C|D> [rep]}"
RUN="${3:-1}"
BENCH="$(cd "$(dirname "$0")" && pwd)"
MAIN="${FUNES_REPO:-$(git -C "$BENCH" rev-parse --show-toplevel 2>/dev/null || true)}"
MODEL="${MODEL:-claude-opus-4-8}"
HANDOFF_MEMORY="${HANDOFF_MEMORY:-dacorvo/funes-handoff-test}"
[ -n "$MAIN" ] && [ -d "$MAIN/.git" ] || { echo "set FUNES_REPO to a funes git clone"; exit 2; }
WTBASE="${WORKTREE_BASE:-$(dirname "$MAIN")}"

# --- fetch the experiment bundle from the dataset (cached; delete the dir to refresh) ---
BUNDLE="${BENCH_BUNDLE:-$WTBASE/funes-$EXP_NAME-bundle}"
if [ ! -f "$BUNDLE/config.sh" ]; then
  echo "fetching experiment bundle artifacts/$EXP_NAME/ from $HANDOFF_MEMORY …"
  tmp="$(mktemp -d)"
  if command -v hf >/dev/null 2>&1; then
    hf download "$HANDOFF_MEMORY" --repo-type dataset --include "artifacts/$EXP_NAME/*" --local-dir "$tmp" >/dev/null 2>&1 || true
  else  # no hf CLI: stdlib fallback (needs a token in HF_TOKEN or ~/.cache/huggingface/token)
    python3 "$BENCH/hub_io.py" download "$HANDOFF_MEMORY" "artifacts/$EXP_NAME" "$tmp" >/dev/null 2>&1 || true
  fi
  [ -f "$tmp/artifacts/$EXP_NAME/config.sh" ] || { echo "no bundle at artifacts/$EXP_NAME/ (config.sh missing) — author one per README.md, or check HF auth"; exit 2; }
  mkdir -p "$BUNDLE"; cp -r "$tmp/artifacts/$EXP_NAME/." "$BUNDLE/"; rm -rf "$tmp"
fi
# shellcheck source=/dev/null
source "$BUNDLE/config.sh"
TASK="$(cat "$BUNDLE/task.md")"

# host requirements (e.g. trufflehog): refuse rather than let a skipping gated test score a false pass
for tool in ${HOST_REQUIRES:-}; do
  command -v "$tool" >/dev/null || { echo "REFUSING: $EXP_NAME needs '$tool' on PATH (HOST_REQUIRES)"; exit 1; }
done

# --- isolation: keep arm claude sessions out of the global ~/.claude/projects (else hooks index them) ---
if [ -n "${BENCH_CLAUDE_DIR:-}" ]; then
  export CLAUDE_CONFIG_DIR="$BENCH_CLAUDE_DIR"
  case "$CLAUDE_CONFIG_DIR" in "$HOME/.claude"|"$HOME/.claude/") echo "REFUSING: CLAUDE_CONFIG_DIR must not be ~/.claude"; exit 1;; esac
  mkdir -p "$CLAUDE_CONFIG_DIR"; PROJROOT="$CLAUDE_CONFIG_DIR/projects"
  echo "isolated CLAUDE_CONFIG_DIR: $CLAUDE_CONFIG_DIR (must be able to auth — e.g. ANTHROPIC_API_KEY)"
else
  if python3 -c "import json,os,sys; d=json.load(open(os.path.expanduser('~/.claude/settings.json'))); sys.exit(0 if d.get('enabledPlugins',{}).get('funes@huggingface') is True else 1)" 2>/dev/null; then
    echo "REFUSING: the funes plugin is ENABLED — disable it first:  claude plugin disable funes@huggingface"; exit 1
  fi
  PROJROOT="$HOME/.claude/projects"; echo "normal config; funes plugin disabled (verified)"
fi

export FUNES_HOME="${BENCH_FUNES_HOME:-$WTBASE/funes-bench-home}"; mkdir -p "$FUNES_HOME"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$WTBASE/funes-bench-target}"
WT="$WTBASE/funes-$EXP_NAME-arm$ARM$RUN"
ENC="$(printf '%s' "$WT" | sed 's#/#-#g')"; PROJ="$PROJROOT/$ENC"
OUT="$BENCH/results/$EXP_NAME/$(date +%F)/$RUN"; mkdir -p "$OUT"

# Auto-memory leak guard. Claude resolves a session's auto-memory from the repo's MAIN worktree, not from
# the arm worktree's cwd — so an arm cut from a clone that HAS project memory silently reads it, and the
# experiment's own answer is usually in there (rerank-triage rep 1 scored 7/7 in 48s citing "my verified
# experimental notes"). Refuse before spending the turn.
MAIN_MEM="$PROJROOT/$(printf '%s' "$MAIN" | sed 's#/#-#g')/memory"
if [ -n "$(ls -A "$MAIN_MEM" 2>/dev/null)" ]; then
  echo "REFUSING: $MAIN has project auto-memory at $MAIN_MEM — every arm would read it."
  echo "  fix: point FUNES_REPO at a clone with no memory of its own, or set BENCH_CLAUDE_DIR."
  exit 1
fi

# don't overwrite a run another host recorded
DATASET_PATH="results/$EXP_NAME/$(date +%F)/$RUN/arm$ARM.jsonl"
if python3 "$BENCH/hub_io.py" exists "$HANDOFF_MEMORY" "$DATASET_PATH" 2>/dev/null; then
  echo "!!! REFUSING: $DATASET_PATH already on the dataset — pick a different rep."; exit 1
fi

# scaffold a worktree at the base commit. The grader is ALWAYS hidden — never handed to the arm, or the
# task degrades to "implement to this handed test" and no channel can beat branch-only (the exp1-easy mistake).
[ -d "$WT" ] || git -C "$MAIN" worktree add --detach "$WT" "$BASE_COMMIT"
echo "pre-warming build (shared target: $CARGO_TARGET_DIR)…"
(cd "$WT" && cargo build --tests) >>"$OUT/prewarm.log" 2>&1 || echo "prewarm had issues (see prewarm.log)"

place_fork() {
  [ -n "${FORK_ID:-}" ] && [ -f "$BUNDLE/fork.jsonl" ] || { echo "arm $ARM needs fork.jsonl + FORK_ID in the bundle"; exit 3; }
  mkdir -p "$PROJ"; cp "$BUNDLE/fork.jsonl" "$PROJ/$FORK_ID.jsonl"
}

STREAM=(--print --verbose --output-format stream-json)
COMMON=(--model "$MODEL" --dangerously-skip-permissions "${STREAM[@]}")
NOFUNES=(--strict-mcp-config --mcp-config '{"mcpServers":{}}')
STEM="arm$ARM"

# Optional wall-clock backstop: an arm has been seen to spawn a background `until ! ps aux | grep
# "cargo test"` watcher that never exits (its own claude cmdline contains "cargo test", so the grep
# always matches) → claude hangs on teardown and stalls the batch. A timeout kills claude, which also
# frees that watcher (the matching cmdline is gone). `timeout` is coreutils, absent on stock macOS, so
# use it only when present; validity is decided by the result event, not by this exit code.
TIMEOUT_BIN="$(command -v timeout || true)"

CLAUDE_EC=0
run_stream() {  # gate on claude's OWN exit (PIPESTATUS[0]); a viewer/tee crash must not discard a completed turn
  cd "$WT"
  set +e
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" --signal=TERM "${CLAUDE_TIMEOUT:-3600}" claude "$@" 2>"$OUT/$STEM.err" | tee "$OUT/$STEM.jsonl" | python3 -u "$BENCH/stream_view.py"
  else
    claude "$@" 2>"$OUT/$STEM.err" | tee "$OUT/$STEM.jsonl" | python3 -u "$BENCH/stream_view.py"
  fi
  local rc=("${PIPESTATUS[@]}"); set -e
  CLAUDE_EC="${rc[0]}"
  [ "$CLAUDE_EC" -eq 124 ] && echo "WARNING: claude hit CLAUDE_TIMEOUT=${CLAUDE_TIMEOUT:-3600}s — likely a self-deadlocked background task; the result-event check below decides validity."
  [ "$CLAUDE_EC" -eq 0 ] || echo "WARNING: claude exited $CLAUDE_EC (stream: ${rc[*]}); see $OUT/$STEM.err"
}

turn_completed() {  # 0 iff the stream holds a successful result event — survives a timeout-kill that happened after the turn finished
  python3 - "$OUT/$STEM.jsonl" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    try: e = json.loads(line)
    except Exception: continue
    if e.get("type") == "result" and e.get("subtype") == "success" and not e.get("is_error"):
        sys.exit(0)
sys.exit(1)
PY
}

memory_clean() {  # 0 iff the arm reported no populated auto-memory dir — the path the harness itself logged, not our slug guess
  python3 - "$OUT/$STEM.jsonl" <<'PY'
import json, os, sys
for line in open(sys.argv[1]):
    try: e = json.loads(line)
    except Exception: continue
    if e.get("type") == "system" and e.get("subtype") == "init":
        p = (e.get("memory_paths") or {}).get("auto")
        if p and os.path.isdir(p) and os.listdir(p):
            print(f"CONTAMINATED: the arm session had auto-memory in context ({p})")
            sys.exit(1)
        break
sys.exit(0)
PY
}

echo "=== $EXP_NAME arm $ARM run $RUN  (base ${BASE_COMMIT:0:9}, worktree $WT) ==="
case "$ARM" in
  A)
    run_stream "${NOFUNES[@]}" "${COMMON[@]}" "$TASK" ;;
  B)
    # The handoff is a frozen doc in the bundle (docs/), produced once out of band and captured — see
    # README. The consumer reads it and does the task; its doc-production cost is added at tabulation.
    mkdir -p "$WT/docs"; cp "$BUNDLE"/docs/*.md "$WT/docs/"
    P='A previous session investigated this and left its design notes in docs/. Read them first, then do the task. '"$TASK"
    run_stream "${NOFUNES[@]}" "${COMMON[@]}" "$P" ;;
  C)
    MCP='{"mcpServers":{"funes":{"command":"funes","args":["mcp","'"$BUNDLE/memory"'"]}}}'
    P='A previous session already investigated this problem, found the root cause, and decided on an approach. You have a recall tool over that session'"'"'s memory — use it to recover the decision, the root cause, and the design before implementing. Then do the task. '"$TASK"
    run_stream --strict-mcp-config --mcp-config "$MCP" "${COMMON[@]}" "$P" ;;
  D)
    place_fork
    P='This repository now lives at the current working directory (a git worktree); ignore any earlier path references and work here. '"$TASK"
    run_stream --resume "$FORK_ID" --fork-session "${NOFUNES[@]}" "${COMMON[@]}" "$P" ;;
  *) echo "unknown arm: $ARM (use A|B|C|D)"; exit 2 ;;
esac

# grade with a HIDDEN copy under a distinct name the arm never saw: it compiles against the base's public
# API, so if the arm changed that surface the grader won't build — a fair fail.
cp "$BUNDLE/grader.rs" "$WT/tests/bench_grader.rs"; GT=bench_grader
echo "=== grading: cargo test --test $GT ==="
if (cd "$WT" && cargo test --test "$GT" -- --nocapture) >"$OUT/$STEM.grade" 2>&1; then echo "GRADE: PASS"
else echo "GRADE: FAIL (see $OUT/$STEM.grade)"; fi

echo "=== usage (final result event) ==="
python3 - "$OUT/$STEM.jsonl" <<'PY'
import json, sys
d=None
for line in open(sys.argv[1]):
    try: e=json.loads(line)
    except Exception: continue
    if e.get("type")=="result": d=e
if not d: print("no result event"); sys.exit(0)
# Tabulate from modelUsage, NOT usage: usage.* reports only part of the turn and undercounts badly and
# unevenly — 442x on hf-cache-recall 2026-08-09/4 arm C (165 vs 72,896 output), 14x on rerank-triage arm A.
# total_cost_usd is computed from modelUsage, so it is the axis that agrees with the dollars.
mu=d.get("modelUsage") or {}
def tot(k): return sum(v.get(k,0) for v in mu.values())
print("turns:", d.get("num_turns"), "cost_usd:", d.get("total_cost_usd"), "duration_ms:", d.get("duration_ms"))
print("uncached_in:", tot("inputTokens"), "cache_create:", tot("cacheCreationInputTokens"),
      "cache_read:", tot("cacheReadInputTokens"), "output:", tot("outputTokens"),
      "(modelUsage; usage.* undercounts)")
PY

rm -rf "$PROJ" 2>/dev/null && echo "removed arm session files: $PROJ" || true

# push receipts ONLY if the turn completed and read no auto-memory — a dead or contaminated run is invalid
if ! turn_completed; then
  echo "NOT syncing to the dataset — no successful result event in the stream (claude exit $CLAUDE_EC); run invalid. Local receipts in $OUT."
elif ! memory_clean; then
  echo "NOT syncing to the dataset — the score is not trustworthy. Local receipts in $OUT."
else
  rc=0
  if command -v hf >/dev/null 2>&1; then
    hf upload "$HANDOFF_MEMORY" "$OUT" "results/$EXP_NAME/$(date +%F)/$RUN" --repo-type dataset --exclude "*prewarm*" >/dev/null 2>&1 || rc=$?
  else
    python3 "$BENCH/hub_io.py" upload "$HANDOFF_MEMORY" "$OUT" "results/$EXP_NAME/$(date +%F)/$RUN" --exclude "*prewarm*" >/dev/null 2>&1 || rc=$?
  fi
  [ "$rc" -eq 0 ] \
    && echo "results synced to dataset: results/$EXP_NAME/$(date +%F)/$RUN" \
    || echo "dataset sync FAILED — push results/$EXP_NAME/$(date +%F)/$RUN manually"
fi

if [ "$ARM" = B ]; then
  echo "NOTE: arm B total = this run + the one-time doc-production charge — add at tabulation."
fi
