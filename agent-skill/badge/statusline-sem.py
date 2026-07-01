#!/usr/bin/env python3
"""Claude Code statusline with a live sem activity badge.

Reads the session JSON on stdin (Claude Code passes it), plus the sem activity
log written by the PostToolUse hook, and renders a one-line status showing what
sem has been doing this session: how many structural queries, the last one, its
latency, and a sparkline of recent latencies. The point is to make the leverage
visible in real time, right on the frontend.
"""
import json
import os
import sys
import time

ACT = os.path.expanduser("~/.claude/sem-activity.jsonl")
SPARK = "▁▂▃▄▅▆▇█"

# ANSI
def c(code):
    return f"\033[{code}m"
RESET, GREEN, DIM, BOLD, CYAN, MAGENTA = c("0"), c("32"), c("2"), c("1"), c("36"), c("35")

TAGLINES = [
    "structural, not textual",
    "the graph, not the grep",
    "entities, not lines",
    "deterministic, not guessed",
]

def spark(latencies):
    if not latencies:
        return ""
    lo, hi = min(latencies), max(latencies)
    span = max(hi - lo, 1)
    return "".join(SPARK[min(int((v - lo) / span * (len(SPARK) - 1)), len(SPARK) - 1)] for v in latencies)

def main():
    try:
        sess = json.load(sys.stdin)
    except Exception:
        sess = {}
    session_id = sess.get("session_id") or sess.get("sessionId") or ""
    cwd = sess.get("workspace", {}).get("current_dir") or sess.get("cwd") or os.getcwd()
    model = (sess.get("model") or {}).get("display_name") or sess.get("model", "")
    dirname = os.path.basename(cwd.rstrip("/")) or "/"

    events = []
    session_events, recent_events = [], []
    cutoff = time.time() - 3 * 3600  # "recent" = last few hours
    if os.path.exists(ACT):
        with open(ACT) as f:
            for line in f:
                try:
                    e = json.loads(line)
                except Exception:
                    continue
                if session_id and e.get("session") == session_id:
                    session_events.append(e)
                if e.get("ts", 0) >= cutoff:
                    recent_events.append(e)
    # prefer this session's activity; fall back to recent so a session-id
    # mismatch between the hook and the statusline can never strand it on "idle"
    events = (session_events or recent_events)[-40:]

    left = f"{DIM}📁 {dirname}{RESET}"
    if model:
        left += f" {DIM}· {model}{RESET}"

    if not events:
        badge = f"{DIM}⊕ sem idle{RESET}"
    else:
        from collections import Counter
        n = len(events)
        last = events[-1]
        lat = [e["ms"] for e in events[-12:] if isinstance(e.get("ms"), (int, float))]
        sp = spark(lat)
        last_tool = last.get("tool", "sem")
        last_target = last.get("target", "")
        op = f"{last_tool} {last_target}".strip() if last_target else last_tool
        last_ms = last.get("ms")
        ms_str = f" {last_ms}ms" if isinstance(last_ms, (int, float)) else ""
        # rotate the trailing segment through real stats + a tagline
        tally = Counter(e.get("tool") for e in events)
        targets = {e.get("target") for e in events if e.get("target")}
        top_cmd, top_n = tally.most_common(1)[0]
        insights = [
            f"{len(targets)} entities analyzed" if targets else TAGLINES[0],
            f"{top_cmd} ×{top_n} top",
            TAGLINES[n % len(TAGLINES)],
        ]
        insight = insights[n % len(insights)]
        badge = (
            f"{GREEN}{BOLD}⊕ sem{RESET} {GREEN}×{n}{RESET}"
            f" {CYAN}{op}{ms_str}{RESET}"
            + (f" {MAGENTA}{sp}{RESET}" if sp else "")
            + f" {DIM}· {insight}{RESET}"
        )

    print(f"{left}  {badge}")

if __name__ == "__main__":
    main()
