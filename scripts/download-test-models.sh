#!/bin/sh
# Fetch weights before tests compete for cache locks (hf-hub waits ten seconds).
set -eu

model_venv="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/funes-test-models"
python3 -m venv "$model_venv"
"$model_venv/bin/python" -m pip install -q "huggingface_hub>=1.21"

for model_repo in "$@"; do
    "$model_venv/bin/hf" download "$model_repo" tokenizer.json model.safetensors
done
