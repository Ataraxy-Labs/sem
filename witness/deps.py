"""Wheels-only third-party dependencies for a Python repo, installed WITHOUT repo code.

Package requirements are parsed on the host from requirements*.txt and pyproject.toml (text parsing only; nothing
of the repo executes). They are installed with `pip install --only-binary=:all:` (no sdist builds, so no setup.py
or build backend of anyone runs) into a target directory, in a container that has network but no repo file
mounted. The witness sandbox then mounts that directory read-only, with no network.
"""
import re
import subprocess
import tomllib
from pathlib import Path

IMAGE = "python:3.12-bookworm"
# too large for a sandboxed witness run: stubbed instead
HEAVY = re.compile(r"^(torch|torchvision|torchaudio|tensorflow.*|jax|jaxlib|nvidia-.*|triton|xformers|vllm|onnxruntime-gpu|deepspeed|bitsandbytes|flash-attn)$")
SKIP_DIRS = {".git", "node_modules", ".venv", "venv", "site-packages", "dist", "build"}


def _req_lines(root: Path):
    out = []
    for p in sorted(root.rglob("requirements*.txt")):
        if any(part in SKIP_DIRS for part in p.parts) or len(p.relative_to(root).parts) > 3:
            continue
        for line in p.read_text(errors="replace").splitlines():
            line = line.split("#", 1)[0].strip()
            if not line or line.startswith(("-", "git+", "http", ".", "/")):
                continue
            out.append(line)
    for p in [root / "pyproject.toml", *sorted(root.glob("*/pyproject.toml"))]:
        if not p.exists():
            continue
        try:
            d = tomllib.loads(p.read_text())
        except Exception:
            continue
        proj = d.get("project", {})
        out += list(proj.get("dependencies", []))
        for v in (proj.get("optional-dependencies") or {}).values():
            out += list(v)
        poetry = d.get("tool", {}).get("poetry", {})
        for name, spec in (poetry.get("dependencies") or {}).items():
            if name.lower() == "python":
                continue
            out.append(name if not isinstance(spec, str) or spec in ("*", "") else f"{name}{_poetry(spec)}")
    seen, reqs = set(), []
    for r in out:
        name = re.split(r"[<>=!~;\[ ]", r, 1)[0].strip().lower()
        if name and re.fullmatch(r"[a-z0-9][a-z0-9._-]*", name) and name not in seen and not HEAVY.match(name):
            seen.add(name)
            reqs.append(r.split(";", 1)[0].strip())
    return reqs


def _poetry(spec: str) -> str:
    spec = spec.strip()
    if spec.startswith("^"):
        return ">=" + spec[1:]
    if spec.startswith("~"):
        return "~=" + spec[1:] if spec.count(".") >= 1 else ">=" + spec[1:]
    return spec if spec[0] in "<>=!" else "==" + spec


def head_date(root: Path) -> str:
    return subprocess.run(["git", "-C", str(root), "log", "-1", "--format=%cI", "HEAD"], capture_output=True, text=True).stdout.strip()


def install(root: Path, target: Path, log: Path) -> dict:
    """Install the requirements as wheels, resolved as of the repo's HEAD commit date (uv --exclude-newer), so
    the environment matches the code's time (not today's incompatible majors); tolerate failures."""
    reqs = _req_lines(root)
    date = head_date(root)
    target.mkdir(parents=True, exist_ok=True)
    (target.parent / f"{target.name}.reqs.txt").write_text("\n".join(reqs) + "\n")
    ok, failed = [], []

    def pip(name, rs):
        cmd = ("pip install -q --target /tmp/uv uv && PYTHONPATH=/tmp/uv python -m uv pip install -q --python python3 "
               f"--only-binary :all: --exclude-newer {date} --target /deps " + " ".join("'" + r.replace("'", "") + "'" for r in rs))
        return subprocess.run(["docker", "run", "--rm", "--name", name,
                               "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--user", "1000:1000",
                               "-e", "HOME=/tmp", "-e", "PIP_DISABLE_PIP_VERSION_CHECK=1", "-v", f"{target}:/deps",
                               IMAGE, "sh", "-c", cmd], capture_output=True, text=True, timeout=1800)

    p = pip(f"witness-deps-all-{abs(hash(str(root))) % 10**8}", reqs) if reqs else None
    if p is not None and p.returncode == 0:
        with open(log, "a") as f:
            f.write(f"== all {len(reqs)} (exclude-newer {date}): rc=0\n")
        return {"requirements": reqs, "installed": reqs, "failed": [], "excludeNewer": date}
    with open(log, "a") as f:
        f.write(f"== all: rc={p.returncode if p else None}\n{(p.stderr if p else '')[-1500:]}\n")
    for r in reqs:
        q = pip(f"witness-deps-{abs(hash((str(root), r))) % 10**8}", [r])
        (ok if q.returncode == 0 else failed).append(r)
        with open(log, "a") as f:
            f.write(f"== {r}: rc={q.returncode}\n{q.stderr[-600:]}\n")
    return {"requirements": reqs, "installed": ok, "failed": failed, "excludeNewer": date}
