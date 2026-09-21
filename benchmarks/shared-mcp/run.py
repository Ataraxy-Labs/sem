#!/usr/bin/env python3
"""Real MCP lifecycle and transport benchmark; no model/token-speed claims.

Usage: python3 benchmarks/shared-mcp/run.py --binary crates/target/debug/sem-mcp
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import select
import signal
import socket
import statistics
import subprocess
import tempfile
import time


def read_reply(process, request_id):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        ready, _, _ = select.select([process.stdout], [], [], max(0, deadline - time.monotonic()))
        if not ready:
            break
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("MCP exited before response")
        reply = json.loads(line)
        if reply.get("id") == request_id:
            if "error" in reply:
                raise RuntimeError(reply)
            return reply["result"]
    raise TimeoutError("MCP response timed out")


def session(binary, repo, shared):
    env = dict(os.environ)
    env.pop("SEM_MCP_SHARED_DAEMON", None)
    if shared:
        env.pop("SEM_MCP_NO_SHARED", None)
        env["SEM_MCP_REQUIRE_SHARED"] = "1"
    else:
        env["SEM_MCP_NO_SHARED"] = "1"
    env["SEM_REPO"] = str(repo)
    start = time.perf_counter()
    process = subprocess.Popen([str(binary)], cwd=repo, env=env,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.DEVNULL)
    def send(method, params, ident=None):
        message = dict(jsonrpc="2.0", method=method, params=params)
        if ident is not None:
            message["id"] = ident
        process.stdin.write((json.dumps(message) + "\n").encode())
        process.stdin.flush()
    try:
        send("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                            "clientInfo": {"name": "shared-benchmark", "version": "1"}}, 1)
        read_reply(process, 1)
        send("notifications/initialized", {})
        send("tools/call", {"name": "sem_context", "arguments": {
            "entity_name": "target_fn", "token_budget": 2000}}, 2)
        result = read_reply(process, 2)
        content = "\n".join(item.get("text", "") for item in result["content"])
        assert not result.get("isError"), content
        assert "def target_fn" in content, "Another client's history suppressed context: " + content
        return {"ms": (time.perf_counter() - start) * 1000,
                "response_bytes": len(content.encode())}
    finally:
        process.stdin.close()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        process.stdout.close()


def stop_daemon(repo, crash=False):
    metadata = repo / ".git/sem/mcp-v2.json"
    if not metadata.exists():
        return
    info = json.loads(metadata.read_text())
    assert info["socket"] == str(repo / ".git/sem/mcp-v2.sock")
    try:
        os.kill(info["pid"], signal.SIGKILL if crash else signal.SIGTERM)
    except ProcessLookupError:
        pass
    # Wait for the kernel to release the daemon's file lock.
    time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=10)
    args = parser.parse_args()
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="sem-mcp-", dir="/tmp") as directory:
        repo = Path(directory).resolve()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        for i in range(40):
            (repo / f"module_{i}.py").write_text("\n".join(
                f"def function_{i}_{j}(value):\n    return value + {j}\n" for j in range(25)))
        (repo / "target.py").write_text("def target_fn(value):\n    return value + 1\n")
        runtime = repo / ".git/sem"
        runtime.mkdir()
        # Seed a crashed daemon socket. Startup must recover it safely.
        stale = socket.socket(socket.AF_UNIX)
        stale.bind(str(runtime / "mcp-v2.sock"))
        stale.close()
        try:
            cold = session(binary, repo, True)
            stop_daemon(repo, crash=True)
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                concurrent_results = list(pool.map(lambda _: session(binary, repo, True), range(8)))
            metadata = json.loads((runtime / "mcp-v2.json").read_text())
            os.kill(metadata["pid"], 0)
            samples = {"standalone_disk_warm": [], "shared_memory_warm": []}
            for i in range(args.repeats):
                # Alternate order to reduce drift bias.
                for shared in ([False, True] if i % 2 == 0 else [True, False]):
                    name = "shared_memory_warm" if shared else "standalone_disk_warm"
                    samples[name].append(session(binary, repo, shared))
            assert json.loads((runtime / "mcp-v2.json").read_text())["pid"] == metadata["pid"]
            stop_daemon(repo, crash=True)
            session(binary, repo, True)
            assert json.loads((runtime / "mcp-v2.json").read_text())["pid"] != metadata["pid"]
            report = {"scope": "synthetic 41-file, 1001-function MCP context retrieval; not agent tasks",
                      "build": str(binary), "concurrent_clients": len(concurrent_results),
                      "stale_socket_recovery": True, "restart_recovery": True,
                      "session_context_isolation": True,
                      "cold_start_ms": cold["ms"],
                      "model_tokens": None, "samples": samples}
            report["summary"] = {name: {"n": len(rows),
                "median_ms": statistics.median(r["ms"] for r in rows),
                "median_response_bytes": statistics.median(r["response_bytes"] for r in rows)}
                for name, rows in samples.items()}
            print(json.dumps(report, indent=2))
        finally:
            stop_daemon(repo)


if __name__ == "__main__":
    main()
