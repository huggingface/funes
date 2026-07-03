#!/usr/bin/env bash
# Run one LongMemEval question through pi + GLM-4.5-Air with a retrieval tool.
#   pi_run.sh <arm: funes|naive> <qid>
# Captures the final answer (answer.txt) + pi session trace (sessions/*.jsonl).
set -uo pipefail
SP="${LME_WORK:-$PWD}"
ARM="$1"; QID="$2"
WROOT="${WROOT:-$SP/work50}"
AROOT="${AROOT:-$SP/agentic}"
FUNESREL="${FUNESREL:-/home/ubuntu/funes/target/release}"
export FUNES_BIN="$FUNESREL/funes"
export PATH="$FUNESREL:$PATH"
export PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_LOCAL_API_KEY=dummy
export FASTEMBED_CACHE_DIR=/home/ubuntu/funes/.fastembed_cache

Q=$(python3 -c "import json;print(json.load(open('$WROOT/$QID/gold.json'))['question'])")
CWD="$AROOT/$ARM/$QID"
rm -rf "$CWD"; mkdir -p "$CWD/sessions"
cd "$CWD"

if [ "$ARM" = "funes" ]; then
  funes add pi >/dev/null 2>&1 && echo "[funes ext installed]" || echo "[funes add pi FAILED]"
  export FUNES_HOME="$WROOT/$QID/store"
  EXT="$CWD/.pi/extensions/funes/index.ts"
else
  mkdir -p .pi/extensions
  cp -r "$SP/naive-rag-ext" .pi/extensions/naive-rag
  export NAIVE_QID="$QID"
  export NAIVE_WORKROOT="$WROOT"
  EXT="$CWD/.pi/extensions/naive-rag/index.ts"
fi
export PI_CODING_AGENT_SESSION_DIR="$CWD/sessions"

PROMPT="Use your available tools to search the user's past conversations, then answer the question concisely. If the answer isn't in their history, say you don't know.

Question: $Q"

timeout 300 pi --provider local --model GLM-4.5-Air --no-builtin-tools -e "$EXT" -p "$PROMPT" >answer.txt 2>run.err
echo "rc=$? arm=$ARM qid=$QID"
echo "Q: $Q"
echo "GOLD: $(python3 -c "import json;print(json.load(open('$WROOT/$QID/gold.json'))['answer'])")"
echo "=== ANSWER ==="; cat answer.txt
echo "=== tool calls ==="; grep -ho '"type":"toolCall"[^]]*' sessions/*.jsonl 2>/dev/null | grep -oE '"name":"[^"]*"' | sort | uniq -c
echo "=== err tail ==="; tail -4 run.err
