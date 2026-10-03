#!/bin/sh
# Edit-time promise feedback for agent harnesses: after an edit, check the
# repo's promises (.sem/promises/*.json) against just the edited file(s).
#
#   promises-hook.sh <file>...        # any harness: pass the edited paths
#   promises-hook.sh < hook.json      # Claude Code: reads tool_input.file_path
#
# Exit 0: promises kept (silent). Exit 2: a promise is broken; the violations
# go to stderr, which Claude Code (PostToolUse) and similar hooks hand back to
# the agent. Any other sem failure is reported but never blocks the edit.
if [ "$#" -eq 0 ] && [ ! -t 0 ]; then
  f=$(sed -n 's/.*"file_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$f" ] && set -- "$f"
fi
[ "$#" -eq 0 ] && exit 0
case $1 in /*) cd "$(dirname "$1")" 2>/dev/null || true ;; esac  # find the file's repo
out=$(sem promises check --changed "$@" 2>&1)
case $? in
  0) exit 0 ;;
  1) printf 'Broken promise after editing %s:\n' "$*" >&2
     printf '%s\n' "$out" | grep -v '^KEPT' >&2; exit 2 ;;
  *) printf '%s\n' "$out" >&2; exit 0 ;;
esac
