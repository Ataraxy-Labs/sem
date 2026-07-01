#!/usr/bin/env python3
"""PostToolUse hook: log sem usage (command + target entity) for the badge.

Fires on both paths sem is used:
  - the sem MCP tools (mcp__sem__sem_*)
  - the sem CLI run through Bash (e.g. `sem impact foo`, `.../release/sem diff`)

Appends {session, ts, tool, target, ms?} to ~/.claude/sem-activity.jsonl, which
the statusline reads to show a live badge of what sem is doing. Exits 0 and
prints nothing so it never interferes with the tool.
"""
import json
import os
import re
import sys
import time

ACT = os.path.expanduser("~/.claude/sem-activity.jsonl")
SUBCMDS = "diff|impact|context|entities|graph|blame|log|orient|xref|mcp|whoami"
CLI_RE = re.compile(r"(?:^|[\s;&|(]|/)sem\s+(" + SUBCMDS + r")\b(.*)")


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return
    tool = data.get("tool_name") or data.get("toolName") or ""
    short = target = None
    ms = None

    if "mcp__sem__" in tool:
        short = tool.split("__")[-1].replace("sem_", "") or "sem"
        ti = data.get("tool_input") or data.get("toolInput") or {}
        if isinstance(ti, dict):
            for k in ("targetEntity", "entity_name", "entity", "query", "path"):
                if ti.get(k):
                    target = str(ti[k])
                    break
        resp = data.get("tool_response") or data.get("toolResponse") or {}
        if isinstance(resp, str):
            try:
                resp = json.loads(resp)
            except Exception:
                resp = {}
        if isinstance(resp, dict):
            for k in ("elapsed_ms", "elapsedMs", "ms"):
                if isinstance(resp.get(k), (int, float)):
                    ms = round(resp[k])
                    break
    elif tool == "Bash":
        cmd = (data.get("tool_input") or data.get("toolInput") or {}).get("command", "")
        m = CLI_RE.search(cmd or "")
        if m:
            short = m.group(1)
            rest = (m.group(2) or "").strip()
            tok = rest.split()[0] if rest else ""
            if tok and not tok.startswith("-"):
                target = tok.strip("'\"")

    if not short:
        return

    event = {
        "session": data.get("session_id") or data.get("sessionId") or "",
        "ts": int(time.time()),
        "tool": short,
    }
    if target:
        event["target"] = target[:40]
    if ms is not None:
        event["ms"] = ms

    try:
        os.makedirs(os.path.dirname(ACT), exist_ok=True)
        with open(ACT, "a") as f:
            f.write(json.dumps(event) + "\n")
    except Exception:
        pass


if __name__ == "__main__":
    main()
