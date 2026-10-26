#!/usr/bin/env python3
"""Publish an immutable release in dependency order, waiting for registry visibility."""
import argparse
import json
import subprocess
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path


class RegistryError(RuntimeError):
    pass


def read_url(url):
    request = urllib.request.Request(url, headers={
        "User-Agent": "ataraxy-release-publisher/1.0", "Cache-Control": "no-cache",
    })
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read().decode()
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise RegistryError(f"Registry HTTP {exc.code}: {url}") from exc
    except (urllib.error.URLError, TimeoutError) as exc:
        raise RegistryError(f"Registry unavailable: {exc}") from exc


def uploaded(name, version):
    body = read_url(f"https://crates.io/api/v1/crates/{name}/{version}")
    if body is None:
        return False
    release = json.loads(body)["version"]
    if release.get("yanked"):
        raise RegistryError(f"{name}@{version} is yanked; refusing to continue")
    return release["num"] == version


def indexed(name, version):
    if len(name) == 1:
        path = f"1/{name}"
    elif len(name) == 2:
        path = f"2/{name}"
    elif len(name) == 3:
        path = f"3/{name[0]}/{name}"
    else:
        path = f"{name[:2]}/{name[2:4]}/{name}"
    body = read_url(f"https://index.crates.io/{path}")
    return body is not None and any(
        row.get("vers") == version and not row.get("yanked", False)
        for row in (json.loads(line) for line in body.splitlines() if line.strip())
    )


def wait_until(check, label, timeout, interval):
    deadline = time.monotonic() + timeout
    while True:
        try:
            if check():
                return
            detail = "not yet visible"
        except RegistryError as exc:
            detail = str(exc)
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RegistryError(f"Timed out waiting for {label}: {detail}. Rerun publication-only when registry indexing recovers.")
        print(f"Waiting for {label}: {detail} ({int(remaining)}s remaining)", flush=True)
        time.sleep(min(interval, remaining))


def publish_one(name, version, command, timeout, interval):
    if uploaded(name, version):
        print(f"{name}@{version} already accepted; skipping upload", flush=True)
    else:
        result = subprocess.run(command, check=False)
        if result.returncode and not uploaded(name, version):
            raise RuntimeError(f"Upload failed for {name}@{version} (exit {result.returncode})")
        wait_until(lambda: uploaded(name, version), f"{name}@{version} upload", timeout, interval)
    wait_until(lambda: indexed(name, version), f"{name}@{version} in Cargo's sparse index", timeout, interval)
    print(f"{name}@{version} is available to dependents", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", default=".")
    parser.add_argument("--crates-dir", default="crates")
    parser.add_argument("--no-verify", action="store_true")
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument("--interval", type=int, default=30)
    parser.add_argument("crates", nargs="+")
    args = parser.parse_args()
    if args.timeout <= 0 or args.interval <= 0:
        parser.error("timeout and interval must be positive")
    for name in args.crates:
        manifest = tomllib.loads((Path(args.crates_dir) / name / "Cargo.toml").read_text())
        if manifest["package"]["name"] != name:
            raise ValueError(f"Package name mismatch for {name}")
        command = ["cargo", "publish", "--manifest-path", str(Path(args.workspace) / "Cargo.toml"), "-p", name]
        if args.no_verify:
            command.append("--no-verify")
        publish_one(name, manifest["package"]["version"], command, args.timeout, args.interval)


if __name__ == "__main__":
    main()
