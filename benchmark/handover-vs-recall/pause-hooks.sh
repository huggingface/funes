#!/usr/bin/env bash
# Isolate this host before running arms: create the PAUSE_INDEX sentinel and guard any installed
# funes hook scripts (claude + codex) so automatic indexing/publishing no-ops while the benchmark's
# fork/arm session files sit under ~/.claude/projects. Idempotent. Reverse with ./unpause-hooks.sh.
set -euo pipefail

SENTINEL="$HOME/.funes/PAUSE_INDEX"
mkdir -p "$HOME/.funes"
[ -f "$SENTINEL" ] || printf 'benchmark pause: automatic funes indexing/publishing disabled.\nrun unpause-hooks.sh to re-enable.\n' >"$SENTINEL"
echo "sentinel: $SENTINEL"

GUARD='[ -f "$HOME/.funes/PAUSE_INDEX" ] && exit 0'
found=0
while IFS= read -r script; do
  [ -n "$script" ] || continue
  found=1
  if grep -qF 'PAUSE_INDEX' "$script"; then
    echo "already guarded: $script"
  else
    tmp="$(mktemp)"
    awk -v g="$GUARD" 'NR==1{print; print "# benchmark pause guard"; print g; next}{print}' "$script" >"$tmp"
    cat "$tmp" >"$script"; rm -f "$tmp"
    echo "guarded: $script"
  fi
done < <(find "$HOME/.claude/plugins" "$HOME/.claude/hooks" "$HOME/.codex/hooks" \( -name funes-index.sh -o -name funes-push.sh -o -name funes-sync.sh -o -name archive-session.sh \) 2>/dev/null)

[ "$found" = 1 ] || echo "no funes hook scripts found (plugin not installed) — the sentinel alone is enough"
