#!/usr/bin/env bash
# Run all 50 questions x 2 arms through pi (agentic), parallel. Resumable: skips
# a (arm,qid) whose answer.txt already exists and is non-empty.
set -uo pipefail
SP="${LME_WORK:-$PWD}"
export WROOT="${WROOT:-$SP/work50}"
export AROOT="${AROOT:-$SP/agentic}"
PAIRS="$AROOT/pairs.txt"; mkdir -p "$AROOT"
: > "$PAIRS"
for d in "$WROOT"/*/; do
  q=$(basename "$d"); [ -d "$d" ] || continue
  for arm in funes naive; do
    a="$AROOT/$arm/$q/answer.txt"
    [ -s "$a" ] || printf '%s %s\n' "$arm" "$q" >> "$PAIRS"
  done
done
echo "to run: $(wc -l < "$PAIRS") (arm,qid) pairs"
t0=$(date +%s)
xargs -P 4 -L1 bash -c 'bash '"$SP"'/pi_run.sh "$0" "$1" >/dev/null 2>&1; echo "done $0 $1"' < "$PAIRS"
echo "BATCH DONE in $(( $(date +%s)-t0 ))s"
