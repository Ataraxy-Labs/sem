"""Witness synthesis loop: propose -> static check -> sandbox run -> verdict, k proposals per task.

    python3 witness/orchestrate.py --tasks T.json --clone DIR --out OUT --ids id1,id2 [--k 3] [--deps DIR]
        [--esbuild NM] [--provider openai-codex] [--model gpt-6.1-sol] [--agent-dir ~/.sem/witness-agent] [--jobs 3]

The proposer is the `pi` coding-agent CLI run without tools (`--provider`/`--model` pick the model it calls);
the sandbox needs docker and the public python:3.12-bookworm / node:24-bookworm-slim images.

Writes OUT/<task id>/attempt-<n>/{prompt.md,response.md,pi.jsonl,harness.*,static.json,run-<i>.json} and
OUT/<task id>/final.json with the tier. A CONFIRMED task also gets OUT/<task id>/witness/ (harness, instrumented
files, the three passing results). CONFIRMED is never written without them.
"""
import argparse
import json
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from anticheat import check  # noqa: E402
from instrument import InstrumentError, instrument  # noqa: E402
from propose import build_prompt, extract_code, propose  # noqa: E402
from sandbox import run_once, verdict  # noqa: E402

RUNS_TO_CONFIRM = 3


def feedback_of(static, res, why):
    if static:
        return "Rejected by the static rules (it did not run):\n" + "\n".join(f"- {v}" for v in static)
    lines = ["The sandbox run failed these checks:", *[f"- {w}" for w in why]]
    for k in ("error", "entry_error"):
        if res.get(k):
            lines += [f"{k}:", res[k][-1500:]]
    if res.get("stubbed_modules"):
        lines.append("auto-stubbed modules: " + ", ".join(res["stubbed_modules"][:40]))
    if res.get("registry"):
        lines.append("functions registered with stubs (W.find keys): " + ", ".join(res["registry"][:40]))
    lines.append(f"canary injected {res.get('injections', 0)} time(s); declared sink call reached {res.get('sink_seen', 0)} time(s); "
                 f"path functions run: {sorted(res.get('path_seen', {}))}; with canary: {sorted(res.get('path_hits', {}))}")
    if res.get("stderr_tail"):
        lines += ["stderr:", res["stderr_tail"][-800:]]
    return "\n".join(lines)


def one_task(task, a, clone: Path, out: Path):
    td = out / task["id"]
    td.mkdir(parents=True, exist_ok=True)
    fin = td / "final.json"
    if fin.exists():
        return json.loads(fin.read_text())
    (td / "task.json").write_text(json.dumps(task, indent=1))
    try:
        inst = instrument(task, clone)
    except InstrumentError as e:
        r = {"id": task["id"], "tier": "UNKNOWN", "why": f"instrumentation failed: {e}", "attempts": []}
        fin.write_text(json.dumps(r, indent=1))
        return r
    attempts, previous, fb = [], None, None
    tier = "POSSIBLE"
    ext = "py" if task["lang"] == "python" else "ts"
    for n in range(1, a.k + 1):
        ad = td / f"attempt-{n}"
        ad.mkdir(exist_ok=True)
        prompt = build_prompt(task, clone, feedback=fb, previous=previous, rules=a.rules)
        (ad / "prompt.md").write_text(prompt)
        text, usage, dt, rc = propose(prompt, Path(a.agent_dir), a.model, ad, provider=a.provider)
        code = extract_code(text, task["lang"])
        rec = {"n": n, "propose_s": dt, "pi_rc": rc, "usage": usage, "runs": [], "static": None}
        if not code:
            rec["static"] = ["no code block in the response"]
            fb = "Your answer had no fenced code block. Answer with one complete harness file in a fenced block."
            attempts.append(rec)
            continue
        (ad / f"harness.{ext}").write_text(code)
        static = check(code, task, a.rules)
        rec["static"] = static
        (ad / "static.json").write_text(json.dumps(static, indent=1))
        previous = code
        if static:
            fb = feedback_of(static, {}, [])
            attempts.append(rec)
            continue
        passed = 0
        for i in range(1, RUNS_TO_CONFIRM + 1):
            res = run_once(task, clone, code, inst, esbuild_nm=Path(a.esbuild) if a.esbuild else None,
                           deps=Path(a.deps) if a.deps else None)
            st, why = verdict(res, task)
            (ad / f"run-{i}.json").write_text(json.dumps({"verdict": st, "why": why, "result": res}, indent=1))
            rec["runs"].append({"verdict": st, "why": why, "seconds": res.get("seconds")})
            if st != "PASS":
                fb = feedback_of(None, res, why) + ("" if i == 1 else f"\n(It passed {i - 1} run(s) before failing: the harness is not deterministic.)")
                break
            passed += 1
        attempts.append(rec)
        if passed == RUNS_TO_CONFIRM:
            tier = "CONFIRMED"
            wd = td / "witness"
            wd.mkdir(exist_ok=True)
            (wd / f"harness.{ext}").write_text(code)
            for rel, text_i in inst.items():
                p = wd / "instrumented" / rel
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(text_i)
            for i in range(1, RUNS_TO_CONFIRM + 1):
                (wd / f"run-{i}.json").write_text((ad / f"run-{i}.json").read_text())
            break
    r = {"id": task["id"], "key": task["key"], "tier": tier, "attempts": attempts,
         "witness": str(td / "witness") if tier == "CONFIRMED" else None}
    assert tier != "CONFIRMED" or all((td / "witness" / f"run-{i}.json").exists() for i in range(1, 4))
    fin.write_text(json.dumps(r, indent=1))
    return r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--clone", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--ids", required=True)
    ap.add_argument("--k", type=int, default=3)
    ap.add_argument("--deps")
    ap.add_argument("--esbuild")
    ap.add_argument("--provider", default="openai-codex", help="pi --provider for the proposer")
    ap.add_argument("--model", default="gpt-6.1-sol", help="pi --model for the proposer")
    ap.add_argument("--agent-dir", default=str(Path.home() / ".sem" / "witness-agent"))
    ap.add_argument("--jobs", type=int, default=3)
    ap.add_argument("--rules", default="v1", choices=["v1", "v2"], help="v2: exploratory relaxations of the TS harness rules")
    a = ap.parse_args()
    tasks = {t["id"]: t for t in json.load(open(a.tasks))["tasks"]}
    ids = [i for i in a.ids.split(",") if i]
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    clone = Path(a.clone)

    def go(i):
        t = time.time()
        r = one_task(tasks[i], a, clone, out)
        print(f"[{time.strftime('%H:%M:%S')}] {i} {r['tier']} attempts={len(r['attempts'])} {time.time() - t:.0f}s", flush=True)
        return r

    with ThreadPoolExecutor(a.jobs) as ex:
        list(ex.map(go, ids))


if __name__ == "__main__":
    main()
