"""Run one witness attempt in a network-less container and judge it.

The container gets: the repo (read-only, copied into a tmpfs /work), the
instrumented files laid over the copy, the harness + task (read-only), the
runtime (read-only), optional wheels-only Python dependencies (read-only),
and a writable /out. No network, no capabilities, no credentials, an
unprivileged user, CPU / memory / pid limits and a wall-clock timeout.
"""
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

RT = Path(__file__).resolve().parent / "rt"
IMAGES = {"python": "python:3.12-bookworm", "ts": "node:24-bookworm-slim"}
TIMEOUT_S = 150


def _task_for_runtime(task):
    keep = ("id", "lang", "pathFunctions", "pythonPath", "tsconfig")
    return {k: task[k] for k in keep if k in task}


def run_once(task, clone: Path, harness: str, instrumented: dict, esbuild_nm: Path | None = None,
             deps: Path | None = None, keep_dir: Path | None = None) -> dict:
    lang = task["lang"]
    tmp = Path(tempfile.mkdtemp(prefix="witness-"))
    try:
        (tmp / "inst").mkdir()
        (tmp / "harness").mkdir()
        (tmp / "out").mkdir()
        os.chmod(tmp / "out", 0o777)
        for rel, text in instrumented.items():
            p = tmp / "inst" / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        hname = "harness.py" if lang == "python" else "harness.ts"
        (tmp / "harness" / hname).write_text(harness)
        (tmp / "harness" / "task.json").write_text(json.dumps(_task_for_runtime(task)))
        rt = tmp / "rt"
        rt.mkdir()
        shutil.copy(RT / "witness_rt.py", rt)
        shutil.copy(RT / "witness_rt.mjs", rt)
        (rt / "node_modules").mkdir()  # mount point of esbuild (read-only)
        name = f"witness-{task['id']}-{uuid.uuid4().hex[:6]}"
        args = ["docker", "run", "--rm", "--name", name, "--network", "none", "--cap-drop", "ALL",
                "--security-opt", "no-new-privileges", "--pids-limit", "256", "--memory", "2g", "--cpus", "2",
                "--user", "1000:1000", "--tmpfs", "/work:rw,exec,size=1500m,mode=1777", "--tmpfs", "/tmp:rw,exec,size=512m,mode=1777",
                "-e", "HOME=/tmp", "-e", "PYTHONDONTWRITEBYTECODE=1", "-e", "NO_COLOR=1",
                "-v", f"{clone}:/src:ro", "-v", f"{tmp / 'inst'}:/inst:ro", "-v", f"{tmp / 'harness'}:/harness:ro",
                "-v", f"{tmp / 'out'}:/out:rw", "-v", f"{rt}:/opt/witness:ro"]
        if lang == "ts":
            args += ["-v", f"{esbuild_nm}:/opt/witness/node_modules:ro"]
        if deps is not None and lang == "python":
            args += ["-v", f"{deps}:/deps:ro", "-e", "PYTHONPATH=/deps"]
        copy = "cd /src && tar -cf - --exclude=.git --exclude=node_modules $(ls -A /src | grep -vx .git) | tar -C /work -xmf - --no-same-owner && cp -r /inst/. /work/"
        run = "python3 /opt/witness/witness_rt.py /harness/harness.py" if lang == "python" else \
              "node /opt/witness/witness_rt.mjs /harness/harness.ts"
        args += [IMAGES[lang], "sh", "-c", f"{copy} && cd /work && {run}"]
        t = time.time()
        try:
            p = subprocess.run(args, capture_output=True, text=True, timeout=TIMEOUT_S)
            rc, so, se = p.returncode, p.stdout, p.stderr
        except subprocess.TimeoutExpired as e:
            subprocess.run(["docker", "kill", name], capture_output=True)
            rc, so, se = -9, str(e.stdout or ""), "timeout"
        dt = time.time() - t
        res_p = tmp / "out" / "result.json"
        res = json.loads(res_p.read_text()) if res_p.exists() else {"error": f"no result (rc={rc})"}
        res["rc"] = rc
        res["seconds"] = round(dt, 2)
        res["stdout_tail"] = re.sub(r"wq[0-9a-f]{16}", "<CANARY>", so[-1500:])
        res["stderr_tail"] = re.sub(r"wq[0-9a-f]{16}", "<CANARY>", se[-1500:])
        if keep_dir is not None:
            keep_dir.mkdir(parents=True, exist_ok=True)
            (keep_dir / "result.json").write_text(json.dumps(res, indent=1))
        return res
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _callee_names(task):
    """Path functions entered through a `calls X` step (they receive the
    value in their arguments)."""
    out = set()
    for s in task.get("path", []):
        m = re.search(r": calls (\S+)$", s)
        if m:
            out.add(m.group(1).split(".")[-1])
    return out


def verdict(res, task):
    """PASS only if every observation rule holds; else the failed rules."""
    why = []
    if res.get("entry_calls", 0) != 1:
        why.append("entry point not called through W.entry")
    if not res.get("injections"):
        why.append("canary never injected at the declared source (source not reached inside the entry call, or its value had no string to carry it)")
    hits = res.get("sink_hits", [])
    good = [h for h in hits if h.get("entryOnStack") and not h.get("badFrames")]
    if not hits:
        why.append(f"canary not observed at the declared sink (sink call reached {res.get('sink_seen', 0)} time(s))")
    elif not good:
        why.append("canary reached the sink only through harness/stub/mock frames or outside the entry call: " + "; ".join(sum((h.get("badFrames", []) for h in hits), [])[:3]))
    if res.get("repo_patches"):
        why.append("the harness patched repo code: " + "; ".join(res["repo_patches"][:3]))
    if res.get("leaks"):
        why.append("canary passed into harness-defined code: " + "; ".join(res["leaks"][:3]))
    callees = _callee_names(task)
    for pf in task.get("pathFunctions", []):
        if pf["role"] != "hop":
            continue
        k = pf["key"]
        need_hit = task["lang"] == "python" or pf["name"] in callees
        if need_hit and not res.get("path_hits", {}).get(k):
            seen = res.get("path_seen", {}).get(k)
            why.append(f"claimed path function {k} {'ran without the canary' if seen else 'never ran'}")
        elif not need_hit and not res.get("path_seen", {}).get(k):
            why.append(f"claimed path function {k} never ran")
    return ("PASS" if not why else "FAIL"), why
