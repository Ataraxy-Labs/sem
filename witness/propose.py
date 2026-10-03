"""The witness proposer: an LLM (via pi, no tools) writes a harness for one task.

The proposer only PROPOSES. It never sees the canary, never runs anything, and its harness is checked statically
and judged by the sandbox runtime; a harness it writes cannot make a flow CONFIRMED unless the canary really
travels from the declared source to the declared sink inside the repo's own code.
"""
import json
import os
import re
import subprocess
import time
from pathlib import Path

from anticheat import forbidden_names

PI_FLAGS = ["--print", "--mode", "json", "--no-session", "--no-tools", "--no-skills", "--no-extensions",
            "--no-context-files", "--no-prompt-templates", "--no-themes", "--offline"]

API_PY = """\
Python harness API (the global `W`; nothing to import):
  W.entry(fn, *args, **kwargs)   call the entry point ONCE; sources/sinks only count inside this call
                                 (async functions are awaited for you)
  W.inject_mode("append"|"prepend"|"replace")   how the runtime puts its secret canary into a string at the source
                                 (default append: value + canary). Choose replace if the value is validated in a way
                                 an appended suffix would break, prepend/append to keep a valid prefix/suffix.
  W.stub_module("pkg.mod", Name=obj, ...)   provide an absent external module (must be called before it is imported)
  W.stub("name")                 a permissive stand-in object for an external dependency (any attr/call works)
  W.add_path("src")              put a repo directory on sys.path (the repo root /work is already on it)
Absent third-party modules are auto-stubbed (permissive objects; a call with one function argument returns that
function, so decorators keep the function). Declared dependencies may be installed for real.
You may set os.environ, create files under /tmp, define plain classes/functions as fakes for EXTERNAL contracts
(they may receive data but must never return or store the source value), and replace attributes of stdlib or
third-party modules (external contracts, e.g. `os.path.exists = lambda p: True`, a fake HTTP client).
"""

API_TS = """\
TS/JS harness API: the file must be exactly `export default async function main(W) { ... }` plus imports.
  await W.entry(fn, ...args)     call the entry point ONCE; sources/sinks only count inside this call
  W.injectMode('append'|'prepend'|'replace')   how the runtime puts its secret canary into a string at the source
  W.stub('name')                 permissive stand-in object (any property/call/new works; awaiting gives itself)
  W.fake({ key: value, method: W.returns(v), asyncMethod: W.resolves(v) })   a fake external object
  W.find('key')                  a function a stubbed framework received: object keys of a router/handler map
                                 (e.g. a tRPC procedure `start` -> W.find('start')) or the stub path
Imports: use absolute paths into the repo copy at /work (e.g. '/work/src/server/x.ts'); the repo's tsconfig paths
work; workspace packages resolve to their source; unresolvable packages become stubs (named imports work).
You may set process.env.X. You may NOT define any other function, arrow function, method or class.
"""

API_TS_V2 = """\
Also allowed:
  await W.entryImport('/work/path/to/module.ts')   INSTEAD of W.entry: the runtime loads that repo module inside the
                                 entry call (use it when the source is in module-level code); counts as the one entry
  globalThis.<name> = W.<api>(...)   e.g. `globalThis.fetch = W.resolves(W.fake({ ok: true, status: 200 }))`
  process.chdir('<literal path>'), process.argv = [...] / process.argv.push(...)   (CLI contracts)
"""

RULES = """\
Rules (checked automatically; a harness that breaks one is rejected without running):
- Call W.entry exactly once. The entry point must be the function holding the declared source (or a repo function
  that calls it); the runtime injects the canary AT the declared source, by itself. You never see the canary and
  must not try to produce it.
- Do not reference these functions of the claimed path by name (they must be reached by the repo's own code from the
  entry point): {forbidden}
- Do not patch repo code (no assignment into repo modules/classes), do not introspect (gc, inspect, sys, frames,
  ctypes, traceback), no threads/processes/subprocess, no unittest.mock, no eval/exec/getattr/setattr, no names
  containing "witness".
- The witness passes only if the canary reaches an argument of the declared sink call through the claimed path
  functions, during the entry call, with no harness code in between.
"""

SYSTEM = """You write small, deterministic test harnesses that drive a repository's own code so that data entering at a
declared SOURCE reaches a declared SINK call along a claimed path. The harness runs in a sandbox with no network.
Answer with ONE fenced code block containing the complete harness file and nothing after it. Before the block you
may write at most five short lines of reasoning."""


def _window(path: Path, line: int, before=15, after=70):
    try:
        lines = path.read_text(errors="replace").splitlines()
    except Exception:
        return ""
    a, b = max(0, line - 1 - before), min(len(lines), line - 1 + after)
    return "\n".join(f"{i + 1:5d}  {lines[i]}" for i in range(a, b))


def _layout(root: Path, lang: str, near: list[str], limit=120):
    exts = (".py",) if lang == "python" else (".ts", ".tsx", ".js", ".mjs", ".jsx")
    files = []
    for d in sorted({str(Path(n).parent) for n in near}):
        base = root / d
        files += [p for p in base.glob("*") if p.suffix in exts]
    tops = [p for p in root.iterdir() if p.is_file() and p.name in ("pyproject.toml", "setup.py", "package.json", "tsconfig.json", "requirements.txt")]
    for t in tops:
        files.append(t)
    rel = sorted({str(p.relative_to(root)) for p in files})[:limit]
    return "\n".join(rel)


def build_prompt(task, root: Path, feedback=None, previous=None, rules="v1"):
    lang = task["lang"]
    src, snk = task["source"], task["sink"]
    files = sorted({src["file"], snk["file"], *[p["file"] for p in task["pathFunctions"]]})
    parts = [f"# Task {task['id']} ({'Python' if lang == 'python' else 'TypeScript/JavaScript'})",
             f"Static claim: {task['key']}",
             "Claimed path (sem's static witness):", *[f"  {s}" for s in task["path"]],
             f"SOURCE: {src['class']} at {src['file']}:{src['line']} in `{src['entity']}` via {src['via']}"
             + (f" -> the runtime injects the canary into parameter `{src['contract']['param']}` when the function starting at line {src['contract'].get('fnLine')} starts" if src['contract']['kind'] == 'param'
                else f" -> the runtime replaces the value of `{src['contract'].get('text', '')}` with a canary-carrying value"),
             f"SINK: {snk['class']} call `{snk['contract']['callText']}` at {snk['file']}:{snk['line']} in `{snk['entity']}` (the runtime watches its arguments)",
             "Path functions (role, file:line):", *[f"  {p['role']:6s} {p['key']} ({p['file']}:{p['line']})" for p in task["pathFunctions"]],
             "", "## Code"]
    shown = set()
    for p in [{"file": src["file"], "line": src["line"]}, *task["pathFunctions"], {"file": snk["file"], "line": snk["line"]}]:
        k = (p["file"], p["line"])
        if k in shown:
            continue
        shown.add(k)
        parts += [f"### {p['file']} around line {p['line']}", "```", _window(root / p["file"], int(p["line"])), "```"]
    for f in files:
        parts += [f"### {f} imports (first 40 lines)", "```", _window(root / f, 1, before=0, after=40), "```"]
    parts += ["## Repository files near the path", "```", _layout(root, lang, files), "```"]
    if lang == "python":
        parts += ["Repo root is /work (on sys.path)."]
    parts += ["", API_PY if lang == "python" else (API_TS + (API_TS_V2 if rules == "v2" else "")), RULES.format(forbidden=", ".join(forbidden_names(task)) or "(none)")]
    if previous is not None:
        parts += ["## Your previous harness", "```", previous, "```", "## What happened", feedback or "",
                  "Fix the harness so the canary reaches the sink along the claimed path. If the path cannot be driven "
                  "(e.g. the sink is unreachable from the source), still return your best harness."]
    parts += ["", f"Write the complete harness file ({'harness.py' if lang == 'python' else 'harness.ts'})."]
    return "\n".join(parts)


def extract_code(text, lang):
    blocks = re.findall(r"```[a-zA-Z0-9_+-]*\n(.*?)```", text, re.S)
    return blocks[-1] if blocks else None


def propose(prompt, agent_dir: Path, model: str, log_dir: Path, thinking="medium", timeout=900, provider="openai-codex"):
    env = dict(os.environ, PI_CODING_AGENT_DIR=str(agent_dir), PI_SKIP_VERSION_CHECK="1", PI_TELEMETRY="0", PI_OFFLINE="1")
    args = ["pi", *PI_FLAGS, "--provider", provider, "--model", model, "--thinking", thinking,
            "--system-prompt", SYSTEM, prompt]
    t = time.time()
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=timeout, env=env, cwd=str(log_dir), stdin=subprocess.DEVNULL)
        out, err, rc = p.stdout, p.stderr, p.returncode
    except subprocess.TimeoutExpired as e:
        out, err, rc = (e.stdout or b"").decode() if isinstance(e.stdout, bytes) else (e.stdout or ""), "timeout", -9
    dt = time.time() - t
    (log_dir / "pi.jsonl").write_text(out)
    (log_dir / "pi.stderr").write_text(err[-4000:])
    usage = {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "cost": 0.0, "turns": 0, "errors": []}
    text = ""
    for line in out.splitlines():
        try:
            e = json.loads(line)
        except Exception:
            continue
        if e.get("type") != "message_end":
            continue
        m = e.get("message", {})
        if m.get("role") != "assistant":
            continue
        u = m.get("usage") or {}
        for k in ("input", "output", "cacheRead", "cacheWrite"):
            usage[k] += int(u.get(k) or 0)
        usage["cost"] += float((u.get("cost") or {}).get("total") or 0)
        usage["turns"] += 1
        if m.get("stopReason") == "error":
            usage["errors"].append(str(m.get("errorMessage", ""))[:300])
        for part in m.get("content") or []:
            if part.get("type") == "text":
                text += part.get("text", "")
    (log_dir / "response.md").write_text(text)
    return text, usage, round(dt, 1), rc
