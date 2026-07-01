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
# `sem` must be in command position (start of line, or right after a shell
# separator), not merely after whitespace — otherwise prose inside quoted
# commit messages ("... sem impact recall, ...") logs garbage events.
CLI_RE = re.compile(
    r"(?:^|[;&|(]\s*|&&\s*|\|\|\s*|\n\s*)(?:[\w./~-]*/)?sem\s+(" + SUBCMDS + r")\b([^\n;&|)]*)"
)
TARGET_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_.:-]*$")


def inside_quotes(text, idx):
    """Cheap heuristic: an odd number of quotes before idx means we're inside
    a quoted string (a commit message, a PR body), not a real invocation."""
    prefix = text[:idx]
    return prefix.count('"') % 2 == 1 or prefix.count("'") % 2 == 1


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return
    tool = data.get("tool_name") or data.get("toolName") or ""
    short = target = None
    ms = None

    file_hint = None

    if "mcp__sem__" in tool:
        short = tool.split("__")[-1].replace("sem_", "") or "sem"
        ti = data.get("tool_input") or data.get("toolInput") or {}
        if isinstance(ti, dict):
            for k in ("targetEntity", "entity_name", "entity", "query", "path"):
                if ti.get(k):
                    target = str(ti[k])
                    break
            if ti.get("file_path"):
                file_hint = str(ti["file_path"])
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
        cmd = (data.get("tool_input") or data.get("toolInput") or {}).get("command", "") or ""
        for m in CLI_RE.finditer(cmd):
            if inside_quotes(cmd, m.start()):
                continue
            short = m.group(1)
            rest = (m.group(2) or "").strip()
            tok = rest.split()[0] if rest else ""
            tok = tok.strip("'\"")
            if tok and not tok.startswith("-") and TARGET_RE.match(tok):
                target = tok
            fm = re.search(r"--file[= ]([^\s]+)", rest)
            if fm:
                file_hint = fm.group(1).strip("'\"")
            break

    if not short:
        return

    event = {
        "session": data.get("session_id") or data.get("sessionId") or "",
        "ts": int(time.time()),
        "tool": short,
    }
    if target:
        event["target"] = target[:40]
    if file_hint:
        event["file"] = file_hint
    cwd = data.get("cwd") or data.get("cwd_path") or ""
    if cwd:
        event["cwd"] = cwd
    if ms is not None:
        event["ms"] = ms

    try:
        os.makedirs(os.path.dirname(ACT), exist_ok=True)
        with open(ACT, "a") as f:
            f.write(json.dumps(event) + "\n")
    except Exception:
        pass

    # Accumulate a persisted lifetime savings tally (single writer: this hook), so
    # the statusline and viewer can show a number that grows across every session.
    # Estimate is anchored to the measured 64-entity benchmark; ~10s and ~900
    # source tokens per avoided grep+read round-trip.
    try:
        save = os.path.expanduser("~/.claude/sem-savings.json")
        rt_per = {"impact": 8, "context": 4, "orient": 5, "diff": 3,
                  "blame": 3, "log": 3, "entities": 2, "xref": 4}
        rt = rt_per.get(short, 2)
        life = {"rt": 0, "sec": 0, "tok": 0, "calls": 0}
        try:
            life.update(json.load(open(save)))
        except Exception:
            pass
        life["rt"] += rt
        life["sec"] += rt * 10
        life["tok"] += rt * 900
        life["calls"] += 1
        json.dump(life, open(save, "w"))
    except Exception:
        pass


if __name__ == "__main__":
    main()
