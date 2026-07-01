#!/usr/bin/env python3
"""PostToolUse hook: log sem MCP tool calls for the statusline badge.

Claude Code passes tool info as JSON on stdin. When the tool is a sem MCP tool
(mcp__sem__sem_*), append a compact event (session, timestamp, short tool name,
and latency if the tool returned elapsed_ms) to ~/.claude/sem-activity.jsonl.
The statusline reads that file to render the live sem badge.

Exit 0 always and print nothing to stdout so we never interfere with the tool.
"""
import json
import os
import sys
import time

ACT = os.path.expanduser("~/.claude/sem-activity.jsonl")

def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return
    tool = data.get("tool_name") or data.get("toolName") or ""
    if "sem" not in tool.lower():
        return
    short = tool.split("__")[-1].replace("sem_", "") or "sem"

    # Pull elapsed_ms out of the tool response if present.
    ms = None
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

    event = {
        "session": data.get("session_id") or data.get("sessionId") or "",
        "ts": int(time.time()),
        "tool": short,
    }
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
