#!/usr/bin/env python3
"""Condense a claude stream-json event stream (on stdin) into a live one-line-per-step view."""
import json
import sys

for line in sys.stdin:
    try:
        e = json.loads(line)
    except Exception:
        continue
    t = e.get("type")
    if t == "assistant":
        for b in e.get("message", {}).get("content", []):
            if b.get("type") == "text" and b.get("text", "").strip():
                print("  [say] " + b["text"].strip()[:200], flush=True)
            elif b.get("type") == "tool_use":
                inp = b.get("input", {}) or {}
                arg = inp.get("command") or inp.get("file_path") or inp.get("pattern") or ""
                print("  [tool] " + str(b.get("name")) + ": " + str(arg)[:140], flush=True)
    elif t == "result":
        print("  [done] turns=" + str(e.get("num_turns")) + " cost=$" + str(e.get("total_cost_usd")), flush=True)
