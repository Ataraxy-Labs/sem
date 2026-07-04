#!/usr/bin/env python3
"""sem-live — a live ASCII blast-radius graph that redraws whenever sem runs.

Run it in a spare terminal pane:

    python3 ~/.claude/sem-live.py

It tails ~/.claude/sem-activity.jsonl (written by the sem PostToolUse hook) and,
each time a new `impact`/`context` call lands, reconstructs that entity's
dependency graph and draws it as an ASCII tree. Other sem ops update the live
activity feed + sparkline at the bottom. Ctrl-C to quit.

Pass --once to render a single frame and exit (used for testing).
"""
import json
import os
import shutil
import subprocess
import sys
import time

ACT = os.path.expanduser("~/.claude/sem-activity.jsonl")

# ANSI
R = "\033[0m"
B = "\033[1m"
DIM = "\033[2m"
GRN = "\033[32m"
CYN = "\033[36m"
MAG = "\033[35m"
YEL = "\033[33m"
RED = "\033[31m"
CLEAR = "\033[2J\033[H"
HIDE = "\033[?25l"
SHOW = "\033[?25h"
SPARK = "▁▂▃▄▅▆▇█"


def sem_bin():
    for cand in (os.environ.get("SEM_BIN"),
                 os.path.expanduser("~/sem/crates/target/release/sem"),
                 shutil.which("sem")):
        if cand and os.path.exists(cand):
            return cand
    return "sem"


SEM = sem_bin()


def read_events():
    events = []
    cutoff = time.time() - 3 * 3600
    if os.path.exists(ACT):
        with open(ACT) as f:
            for line in f:
                try:
                    e = json.loads(line)
                except Exception:
                    continue
                if e.get("ts", 0) >= cutoff:
                    events.append(e)
    return events[-60:]


def spark(vals):
    if not vals:
        return ""
    lo, hi = min(vals), max(vals)
    rng = (hi - lo) or 1
    return "".join(SPARK[min(len(SPARK) - 1, int((v - lo) / rng * (len(SPARK) - 1)))] for v in vals)


SAVE = os.path.expanduser("~/.claude/sem-savings.json")

# Savings model, anchored to the measured 64-entity closure benchmark:
# grep+read took 17 round-trips / 180s / 35.7k tokens; sem took 2 / 27s / 21.5k.
# So a big structural query saves ~15 round-trips; each round-trip is ~one LLM
# inference cycle (~10s) and ~900 tokens of source an agent would otherwise read.
# Everything is labelled "≈" — these are honest estimates, not precise counts.
RT_PER_TOOL = {"impact": 8, "context": 4, "orient": 5, "diff": 3,
               "blame": 3, "log": 3, "entities": 2, "xref": 4}
SEC_PER_RT = 10
TOK_PER_RT = 900


def rt_saved(tool, total=None):
    if tool == "impact" and total:
        return max(2, round(total * 15 / 64))  # scale to the measured benchmark
    return RT_PER_TOOL.get(tool, 2)


def fmt_time(sec):
    sec = int(sec)
    if sec < 90:
        return f"{sec}s"
    if sec < 3600:
        return f"{sec // 60}m"
    return f"{sec // 3600}h {(sec % 3600) // 60}m"


def fmt_num(n):
    n = int(n)
    if n >= 1000:
        return f"{n / 1000:.1f}k".replace(".0k", "k")
    return str(n)


def update_lifetime(events):
    """Read the persisted lifetime tally. The PostToolUse hook is the single writer
    (it bumps this on every sem call), so the viewer only reads — that keeps the
    lifetime counter growing from real usage even when the viewer isn't open, and
    avoids double-counting."""
    try:
        return json.load(open(SAVE))
    except Exception:
        return {"rt": 0, "sec": 0, "tok": 0, "calls": 0}


_graph_cache = {}


def fetch_graph(ev):
    """Run sem impact for the event's target and return (direct, total, entity, file)."""
    target = ev.get("target")
    if not target:
        return None
    key = (ev.get("cwd", ""), target, ev.get("file", ""))
    if key in _graph_cache:
        return _graph_cache[key]
    cmd = [SEM, "impact", target, "--depth", "0", "--json"]
    if ev.get("file"):
        cmd += ["--file", ev["file"]]
    cwd = ev.get("cwd") or None
    try:
        out = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=8).stdout
        d = json.loads(out)
    except Exception:
        _graph_cache[key] = None
        return None
    direct = [(x.get("name", "?"), x.get("file", "")) for x in d.get("dependents", [])]
    total = d.get("impact", {}).get("total", len(direct))
    ent = d.get("entity", target)
    if isinstance(ent, dict):
        ent = ent.get("name", target)
    file = d.get("file", ev.get("file", ""))
    if isinstance(file, dict):
        file = file.get("file", ev.get("file", ""))
    res = (direct, total, ent, file)
    _graph_cache[key] = res
    return res


def shorten(path, width=34):
    if len(path) <= width:
        return path
    parts = path.split("/")
    out = parts[-1]
    for p in reversed(parts[:-1]):
        if len(out) + len(p) + 1 > width:
            return "…/" + out
        out = p + "/" + out
    return out


def is_test(name, file):
    # Rust test fns are often long behaviour-describing snake_case names with no
    # `test_` prefix, so also treat a many-underscore name as a test for display
    # ranking — this keeps real callers (run_diff_pipeline, sem_diff) on top.
    return (name.startswith("test_") or name.startswith("test")
            or "/tests/" in file or file.endswith("_test.rs")
            or name.count("_") >= 4)


def render(events, width):
    lines = []
    clock = time.strftime("%H:%M:%S")
    head = f"{GRN}{B}⊕ sem live{R}  {DIM}watching {os.path.basename(ACT)}{R}"
    lines.append(head + " " * max(1, width - 26 - len(clock)) + f"{DIM}{clock}{R}")
    lines.append(DIM + "═" * width + R)

    if not events:
        lines.append("")
        lines.append(f"  {DIM}idle — run sem in the other pane and this lights up{R}")
        return "\n".join(lines)

    last = events[-1]
    tool = last.get("tool", "sem")
    target = last.get("target", "")
    ms = last.get("ms")
    ms_s = f"  {YEL}{ms}ms{R}" if isinstance(ms, (int, float)) else ""
    op = f"{CYN}{B}{tool}{R} {CYN}{target}{R}" if target else f"{CYN}{B}{tool}{R}"
    lines.append("")
    lines.append(f"  {op}{ms_s}")

    # Draw the blast radius of the most recent ANALYZABLE event (impact or
    # context with a real target), even when later diffs/logs came after it —
    # otherwise the graph flickers out every time an unrelated command runs.
    def analyzable(e):
        return e.get("tool") in ("impact", "context") and e.get("target")

    graph_ev = next((e for e in reversed(events) if analyzable(e)), None)
    graph = fetch_graph(graph_ev) if graph_ev else None
    if graph and graph_ev is not last:
        t = graph_ev.get("tool", "")
        lines.append(f"  {DIM}last analyzed · {t} {graph_ev.get('target','')}{R}")
    if graph:
        direct, total, entity, file = graph
        lines.append(f"  {DIM}{'─' * (width - 4)}{R}")
        lines.append(f"  {GRN}{B}◉ {entity}{R}   {DIM}{shorten(file)}{R}")
        if not direct:
            lines.append(f"  {DIM}╰── no callers — nothing in this repo depends on it{R}")
        else:
            lines.append(f"  {DIM}│{R}  {YEL}{len(direct)} direct{R} {DIM}→{R} {YEL}{total} transitive{R}")
        # non-test first, so the meaningful callers are on top
        ordered = sorted(direct, key=lambda x: (is_test(x[0], x[1]), x[0]))
        shown = ordered[:9]
        tests = sum(1 for n, f in ordered if is_test(n, f))
        for i, (name, f) in enumerate(shown):
            elbow = "╰─▶" if i == len(shown) - 1 and len(ordered) <= 9 else "├─▶"
            col = DIM if is_test(name, f) else CYN
            lines.append(f"  {DIM}{elbow}{R} {col}{name}{R}"
                         + " " * max(1, 30 - len(name)) + f"{DIM}{shorten(f, 30)}{R}")
        if len(ordered) > 9:
            extra = len(ordered) - 9
            note = f"+{extra} more" + (f" ({tests} tests)" if tests else "")
            lines.append(f"  {DIM}╰─▶ … {note}{R}")
    elif graph_ev:
        lines.append(f"  {DIM}(no graph — could not resolve {graph_ev.get('target','?')} from {graph_ev.get('cwd','?')}){R}")

    # savings meter — the "you're saving so much" panel
    sess_rt = sum(rt_saved(e.get("tool")) for e in events)
    life = update_lifetime(events)
    call_rt = rt_saved(tool, graph[1] if graph else None)
    lines.append("")
    lines.append(f"  {GRN}◇ you're saving{R}  {DIM}vs grep + read (≈ estimated){R}")
    lines.append(f"  {DIM}this call{R}   grep ≈ {YEL}{call_rt} round-trips{R} {DIM}· sem did it in{R} {GRN}1{R}")
    lines.append(f"  {DIM}session  {R}  {len(events)} calls {DIM}·{R} ≈ {YEL}{sess_rt}{R} round-trips {DIM}·{R} ≈ {YEL}{fmt_time(sess_rt * SEC_PER_RT)}{R} {DIM}·{R} ≈ {YEL}{fmt_num(sess_rt * TOK_PER_RT)}{R} tokens spared")
    lines.append(f"  {DIM}lifetime {R}  {life['calls']} calls {DIM}·{R} ≈ {GRN}{B}{fmt_num(life['rt'])} round-trips{R} {DIM}·{R} ≈ {GRN}{B}{fmt_time(life['sec'])}{R} saved")

    # activity feed + sparkline
    lines.append("")
    lines.append(f"  {DIM}{'─' * (width - 4)}{R}")
    tally = {}
    for e in events:
        tally[e.get("tool", "?")] = tally.get(e.get("tool", "?"), 0) + 1
    lat = [e["ms"] for e in events[-16:] if isinstance(e.get("ms"), (int, float))]
    feed = "  ".join(f"{t}{DIM}×{n}{R}" for t, n in sorted(tally.items(), key=lambda x: -x[1])[:6])
    lines.append(f"  {MAG}{spark(lat)}{R}   {feed}")
    recent = events[-6:]
    trail = " ".join(f"{DIM}{e.get('tool','?')}"
                     + (f":{e.get('target','')[:12]}" if e.get('target') else "") + R
                     for e in recent)
    lines.append(f"  {DIM}recent:{R} {trail}")
    return "\n".join(lines)


def term_width():
    try:
        return min(shutil.get_terminal_size().columns, 100)
    except Exception:
        return 80


def main():
    once = "--once" in sys.argv
    if once:
        print(render(read_events(), term_width()))
        return
    sys.stdout.write(HIDE)
    last_mtime = 0
    last_draw = 0
    try:
        while True:
            mtime = os.path.getmtime(ACT) if os.path.exists(ACT) else 0
            now = time.time()
            if mtime != last_mtime or now - last_draw >= 1:
                frame = render(read_events(), term_width())
                sys.stdout.write(CLEAR + frame + "\n")
                sys.stdout.flush()
                last_mtime = mtime
                last_draw = now
            time.sleep(0.3)
    except KeyboardInterrupt:
        evs = read_events()
        rt = sum(rt_saved(e.get("tool")) for e in evs)
        sys.stdout.write(CLEAR)
        print(f"{GRN}{B}⊕ sem{R}  this session: {len(evs)} calls {DIM}·{R} "
              f"≈ {YEL}{rt}{R} grep round-trips avoided {DIM}·{R} "
              f"≈ {GRN}{fmt_time(rt * SEC_PER_RT)}{R} {DIM}·{R} "
              f"≈ {GRN}{fmt_num(rt * TOK_PER_RT)}{R} tokens spared")
        try:
            life = json.load(open(SAVE))
            if life.get("rt"):
                print(f"{DIM}   lifetime with sem: ≈ {fmt_num(life['rt'])} round-trips · "
                      f"≈ {fmt_time(life['sec'])} saved. keep going.{R}")
        except Exception:
            pass
    finally:
        sys.stdout.write(SHOW + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()
