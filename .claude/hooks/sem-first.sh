#!/usr/bin/env python3
"""PreToolUse hook: make sem the prevalent tool for code-symbol search.

When the agent runs the Grep tool with a bare code identifier (a function /
type / variable name), block it and steer to the sem MCP tools, which answer
structural questions grep cannot (cross-file callers/callees, blast radius)
without false positives.

Deliberately NOT a hard wall:
  - Only Grep-tool calls whose pattern is a bare identifier are redirected.
  - Real text search (regex, quoted strings, error messages, config keys),
    discovery by unknown name, and non-code files pass straight through.
  - Bash `grep`/`find` are never intercepted, so there's always an escape hatch.
This matches sem's calibrated role: sem for structure, grep for text.
"""
import sys, json, re

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)  # never block on a parse error

tool = data.get("tool_name", "")
pattern = (data.get("tool_input") or {}).get("pattern", "") or ""

# A "bare code identifier": starts with a letter/underscore, only word chars,
# at least 3 chars. Anything with regex metacharacters, spaces, or quotes is
# treated as genuine text search and allowed through.
is_symbol = re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{2,}", pattern) is not None

if tool == "Grep" and is_symbol:
    reason = (
        f'"{pattern}" looks like a code symbol. Prefer sem (the entity graph) over grep here:\n'
        f'  - sem_impact  — what depends on / calls "{pattern}" (blast radius, cross-file, no false positives)\n'
        f'  - sem_context — pull "{pattern}" plus its real callers and callees, token-budgeted\n'
        f'  - sem_entities — locate "{pattern}" and its definition (file:line)\n'
        "Grep is still the right tool for text/string search, error messages, config keys, "
        "discovery by an unknown name, and non-code files. If this is genuinely a text search, "
        "re-run it with Bash `grep` (not intercepted)."
    )
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    }))
    sys.exit(0)

sys.exit(0)
