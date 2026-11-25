#!/bin/sh
# Fetch weights before tests compete for cache locks (hf-hub waits ten seconds).
set -eu

model_venv="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/funes-test-models"
# Windows, under Git's sh: RUNNER_TEMP is D:\a\_temp, which D:/a/_temp spells for both sides.
if command -v cygpath >/dev/null 2>&1; then model_venv=$(cygpath -m "$model_venv"); fi
# Windows has python, and a python3 that may only point at the Store.
for python in python3 python; do
    "$python" -c '' 2>/dev/null && break
done
"$python" -m venv "$model_venv"
# A Windows venv keeps its programs in Scripts.
bin="$model_venv/bin"
[ -d "$bin" ] || bin="$model_venv/Scripts"
"$bin/python" -m pip install -q "huggingface_hub>=1.21"

for model_repo in "$@"; do
    "$bin/hf" download "$model_repo" tokenizer.json model.safetensors
done
