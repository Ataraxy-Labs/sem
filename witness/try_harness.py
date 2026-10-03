"""Run a given harness for one task N times and print the verdicts.

    python3 witness/try_harness.py TASKS.json TASK_ID CLONE HARNESS [N] [--deps DIR] [--esbuild NM]
"""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from anticheat import check  # noqa: E402
from instrument import instrument  # noqa: E402
from sandbox import run_once, verdict  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("tasks")
ap.add_argument("task_id")
ap.add_argument("clone")
ap.add_argument("harness")
ap.add_argument("n", nargs="?", type=int, default=1)
ap.add_argument("--deps")
ap.add_argument("--esbuild")
ap.add_argument("--no-static", action="store_true", help="skip the static check (cheat-suite defence-in-depth test only)")
ap.add_argument("--json-out")
ap.add_argument("--rules", default="v1")
a = ap.parse_args()
tasks = {t["id"]: t for t in json.load(open(a.tasks))["tasks"]}
task = tasks[a.task_id]
text = Path(a.harness).read_text()
v = check(text, task, a.rules)
print("anticheat:", v or "ok")
summary = {"static": v, "runs": []}
if v and not a.no_static:
    if a.json_out:
        Path(a.json_out).write_text(json.dumps(summary, indent=1))
    sys.exit(1)
inst = instrument(task, Path(a.clone))
for i in range(a.n):
    res = run_once(task, Path(a.clone), text, inst, esbuild_nm=Path(a.esbuild) if a.esbuild else None,
                   deps=Path(a.deps) if a.deps else None)
    st, why = verdict(res, task)
    print(f"run {i + 1}: {st} {why} ({res['seconds']}s)")
    summary["runs"].append({"verdict": st, "why": why, "result": res})
    if st != "PASS":
        print(json.dumps({k: res.get(k) for k in ("error", "entry_error", "injections", "sink_seen", "sink_hits", "path_hits", "path_seen", "leaks", "stubbed_modules", "stderr_tail")}, indent=1)[:4000])
if a.json_out:
    Path(a.json_out).write_text(json.dumps(summary, indent=1))
