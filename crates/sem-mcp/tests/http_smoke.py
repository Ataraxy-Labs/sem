"""Live HTTP regression test, using only the Python standard library.

Build sem-mcp, then: python3 crates/sem-mcp/tests/http_smoke.py /path/to/sem-mcp
"""
import json
import http.client
import os
from pathlib import Path
import queue
import secrets
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.error
import urllib.request
from urllib.parse import urlsplit

BINARY = str(Path(sys.argv.pop(1)).resolve())


class HTTPTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory(prefix="sem-http-test-")
        cls.addClassCleanup(cls.tmp.cleanup)
        root = Path(cls.tmp.name)
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        (root / "config.py").write_text("def parse_config(value):\n    return value.strip()\n")
        subprocess.run(["git", "-C", str(root), "add", "config.py"], check=True)
        cls.token = secrets.token_hex(32)
        env = dict(os.environ, SEM_REPO=str(root), SEM_MCP_HTTP_TOKEN=cls.token,
                   SEM_NO_NETWORK="1", SEM_NO_TELEMETRY="1")
        cls.process = subprocess.Popen([BINARY, "--http", "127.0.0.1:0"], env=env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        cls.addClassCleanup(cls.stop)
        lines = queue.Queue()
        def drain():
            for line in cls.process.stderr:
                if line.startswith("Sem read-only MCP: "):
                    lines.put(line)
        threading.Thread(target=drain, daemon=True).start()
        cls.url = lines.get(timeout=30).split()[3]

    @classmethod
    def stop(cls):
        cls.process.terminate()
        try:
            cls.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            cls.process.kill()
            cls.process.wait()
        cls.process.stderr.close()

    def request(self, message, **headers):
        base = {"Authorization": "Bearer " + self.token, "Content-Type": "application/json",
                "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-03-26"}
        base.update(headers)
        body = json.dumps(message).encode()
        request = urllib.request.Request(self.url, body, base)
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as response:
            return response.code, response.read()

    def rpc(self, method, params=None):
        message = {"jsonrpc": "2.0", "id": 1, "method": method}
        if params is not None:
            message["params"] = params
        status, body = self.request(message)
        self.assertEqual(status, 200, body)
        return json.loads(body)

    def test_initialize(self):
        result = self.rpc("initialize", {"protocolVersion": "2025-03-26", "capabilities": {},
                                        "clientInfo": {"name": "sem-http-test", "version": "1"}})
        self.assertIn("tools", result["result"]["capabilities"])

    def test_discovery_and_query(self):
        tools = self.rpc("tools/list")["result"]["tools"]
        self.assertEqual({t["name"] for t in tools}, {"sem_find", "sem_grep"})
        for _ in range(2):
            result = self.rpc("tools/call", {"name": "sem_find", "arguments": {"files": ["config.py"]}})
            self.assertFalse(result["result"].get("isError", False), result)
            self.assertIn("return value.strip()", json.dumps(result))

    def test_hidden_tools_rejected(self):
        for name in ["sem_check", "sem_graph", "join_review", "sem_certify"]:
            result = self.rpc("tools/call", {"name": name, "arguments": {}})
            self.assertEqual(result["error"]["code"], -32602)

    def test_entity_context_and_grep(self):
        result = self.rpc("tools/call", {"name": "sem_find", "arguments": {"query": "parse_config", "mode": "context", "token_budget": 1200}})
        self.assertFalse(result["result"].get("isError", False), result)
        self.assertIn("return value.strip()", json.dumps(result))
        result = self.rpc("tools/call", {"name": "sem_grep", "arguments": {"pattern": "parse_config"}})
        self.assertFalse(result["result"].get("isError", False), result)
        self.assertIn("config.py", json.dumps(result))

    def test_paths_rejected(self):
        for path in ["../secret", "/etc/passwd"]:
            result = self.rpc("tools/call", {"name": "sem_find", "arguments": {"files": [path]}})
            self.assertEqual(result["error"]["code"], -32602)

    def test_http_guards(self):
        message = {"jsonrpc": "2.0", "id": 1, "method": "tools/list"}
        for headers, expected in [({"Authorization": ""}, 401), ({"Origin": "https://evil.example"}, 403),
                                  ({"Host": "evil.example"}, 403)]:
            self.assertEqual(self.request(message, **headers)[0], expected)
        # Send headers first: a server rejecting Content-Length can close before
        # urllib finishes writing a large body (connection reset on macOS).
        url = urlsplit(self.url)
        connection = http.client.HTTPConnection(url.hostname, url.port, timeout=10)
        try:
            connection.putrequest("POST", url.path)
            connection.putheader("Authorization", "Bearer " + self.token)
            connection.putheader("Content-Type", "application/json")
            connection.putheader("Content-Length", str(1024 * 1024 + 1))
            connection.endheaders()
            self.assertEqual(connection.getresponse().status, 413)
        finally:
            connection.close()

    def test_nonloopback_rejected(self):
        result = subprocess.run([BINARY, "--http", "0.0.0.0:8765"], capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"loopback", result.stderr)


if __name__ == "__main__":
    unittest.main()
