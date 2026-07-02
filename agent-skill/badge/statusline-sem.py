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
YELLOW = c("33")

SAVE = os.path.expanduser("~/.claude/sem-savings.json")
# Savings model, anchored to the measured 64-entity closure benchmark (grep+read
# 17 round-trips / 180s / 35.7k tokens vs sem 2 / 27s / 21.5k). Each avoided
# round-trip is ~one LLM inference cycle (~10s) and ~900 tokens of source read.
RT_PER_TOOL = {"impact": 8, "context": 4, "orient": 5, "diff": 3,
               "blame": 3, "log": 3, "entities": 2, "xref": 4}

def rt_saved(tool):
    return RT_PER_TOOL.get(tool, 2)

def fmt_time(sec):
    sec = int(sec)
    if sec < 90:
        return f"{sec}s"
    if sec < 3600:
        return f"{sec // 60}m"
    return f"{sec // 3600}h{(sec % 3600) // 60}m"

def fmt_num(n):
    n = int(n)
    return f"{n / 1000:.1f}k".replace(".0k", "k") if n >= 1000 else str(n)

def read_lifetime():
    try:
        return json.load(open(SAVE))
    except Exception:
        return {}


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
        life = read_lifetime()
        if life.get("sec"):
            badge = (f"{DIM}⊕ sem idle{RESET} {DIM}·{RESET} "
                     f"{YELLOW}≈ {fmt_time(life['sec'])} · ≈ {fmt_num(life['tok'])} tokens saved{RESET}")
        else:
            badge = f"{DIM}⊕ sem idle{RESET}"
    else:
        done = [e for e in events if e.get("phase") != "start"]
        n = len(done)
        last = events[-1]
        # In-flight: the newest event is a fresh start with no completion yet —
        # show it the moment the agent triggers sem, with a live spinner.
        in_flight = last.get("phase") == "start" and time.time() - last.get("ts", 0) < 120
        SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
        show = last if (in_flight or not done) else done[-1]
        tool_name = show.get("tool", "sem")
        tgt = show.get("target", "")
        op = f"{tool_name} {tgt}".strip() if tgt else tool_name
        ms = show.get("ms")
        ms_str = f" {ms}ms" if isinstance(ms, (int, float)) else ""
        sess_rt = sum(rt_saved(e.get("tool")) for e in done)
        saved = f"≈ {fmt_time(sess_rt * 10)} · ≈ {fmt_num(sess_rt * 900)} tokens saved"
        if in_flight:
            spin = SPIN[int(time.time() * 8) % len(SPIN)]
            badge = (
                f"{GREEN}{BOLD}⊕ sem{RESET} {YELLOW}{BOLD}{spin} {op}…{RESET}"
                + (f" {DIM}·{RESET} {YELLOW}{saved}{RESET}" if n else "")
            )
        else:
            badge = (
                f"{GREEN}{BOLD}⊕ sem{RESET} {GREEN}×{n}{RESET}"
                f" {CYAN}{op}{ms_str}{RESET}"
                f" {DIM}·{RESET} {YELLOW}{saved}{RESET}"
            )

    print(f"{left}  {badge}")

if __name__ == "__main__":
    main()
