"""Real CLI smoke tests; no model calls or access to the user's repositories."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class Workflows(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sem-plugin-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.env = {
            **os.environ,
            "SEM_NO_NETWORK": "1",
            "SEM_NO_TELEMETRY": "1",
            "SEM_CACHE_DIR": str(self.root / "cache"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
        }
        self.run_cli("git", "init", "-q", "-b", "main")
        self.run_cli("git", "config", "user.name", "Plugin smoke test")
        self.run_cli("git", "config", "user.email", "test@example.invalid")
        self.source = self.repo / "config.py"
        self.source.write_text(
            "def parse_config(text):\n    return text.strip()\n\n"
            "def load_config(text):\n    return parse_config(text)\n",
            encoding="utf-8",
        )
        self.run_cli("git", "add", "config.py")
        self.run_cli("git", "commit", "-qm", "base")

    def run_cli(self, *args):
        result = subprocess.run(
            args, cwd=self.repo, env=self.env, text=True, capture_output=True, timeout=60
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout

    def snapshot(self):
        return (
            self.run_cli("git", "rev-parse", "HEAD"),
            self.run_cli("git", "ls-files", "--stage"),
            self.run_cli("git", "status", "--porcelain"),
            self.source.read_bytes(),
        )

    @unittest.skipUnless(shutil.which("sem"), "Ataraxy Sem is required")
    def test_sem_discovery_and_diff_preserve_worktree(self):
        self.assertIn("0.27.", self.run_cli("sem", "--version"))
        self.source.write_text(self.source.read_text().replace("text.strip()", "text.strip().lower()"))
        before = self.snapshot()
        for args, expected in [
            (("find", "parse_config", "--json"), "parse_config"),
            (("find", "parse_config", "--callers", "--json"), "load_config"),
            (("find", "parse_config", "--context", "--budget", "3000", "--json"), "parse_config"),
            (("diff", "--json"), "parse_config"),
        ]:
            with self.subTest(args=args):
                result = json.loads(self.run_cli("sem", *args))
                self.assertIn(expected, json.dumps(result))
        self.assertEqual(before, self.snapshot())

    @unittest.skipUnless(shutil.which("weave"), "Ataraxy Weave is required")
    def test_weave_preview_preserves_worktree(self):
        self.run_cli("git", "switch", "-qc", "feature-a")
        self.source.write_text(self.source.read_text().replace("text.strip()", "text.strip().lower()"))
        self.run_cli("git", "commit", "-qam", "normalize config")
        self.run_cli("git", "switch", "-q", "main")
        self.source.write_text(self.source.read_text().replace("return parse_config(text)", "return parse_config(str(text))"))
        self.run_cli("git", "commit", "-qam", "accept other input values")
        before = self.snapshot()
        output = self.run_cli("weave", "preview", "feature-a")
        self.assertIn("config.py", output)
        self.assertEqual(before, self.snapshot())


if __name__ == "__main__":
    unittest.main()
