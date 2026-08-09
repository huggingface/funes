#!/usr/bin/env bash
# Reverse pause-hooks.sh: remove the benchmark guard from funes hook scripts and delete the
# sentinel, re-enabling automatic indexing/publishing. Run only after benchmark cleanup
# (fork + arm session files removed from ~/.claude/projects), or the next index sweeps them in.
set -euo pipefail

found=0
while IFS= read -r script; do
  [ -n "$script" ] || continue
  found=1
  if grep -qF 'benchmark pause guard' "$script"; then
    tmp="$(mktemp)"
    grep -v 'benchmark pause guard' "$script" | grep -v 'PAUSE_INDEX' >"$tmp"
    cat "$tmp" >"$script"; rm -f "$tmp"
    echo "unguarded: $script"
  fi
done < <(find "$HOME/.claude/plugins" "$HOME/.claude/hooks" "$HOME/.codex/hooks" \( -name funes-index.sh -o -name funes-push.sh -o -name funes-sync.sh -o -name archive-session.sh \) 2>/dev/null)
[ "$found" = 1 ] || echo "no funes hook scripts found"

rm -f "$HOME/.funes/PAUSE_INDEX" && echo "removed sentinel ~/.funes/PAUSE_INDEX"
echo "reminder: only unpause once fork + arm session files are cleared from ~/.claude/projects"
