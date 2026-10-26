import importlib.util
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("publisher", Path(__file__).with_name("publish-crates.py"))
p = importlib.util.module_from_spec(spec)
spec.loader.exec_module(p)


class PublicationTests(unittest.TestCase):
    def test_accepted_upload_waits_for_index_without_reupload(self):
        with patch.object(p, "uploaded", return_value=True), patch.object(p, "indexed", side_effect=[False, True]) as index, patch.object(p.subprocess, "run") as run, patch.object(p.time, "sleep"):
            p.publish_one("test-crate", "1.0.0", ["cargo"], 60, 1)
        run.assert_not_called()
        self.assertEqual(index.call_count, 2)

    def test_auth_failure_stops_before_index(self):
        with patch.object(p, "uploaded", return_value=False), patch.object(p.subprocess, "run", return_value=SimpleNamespace(returncode=101)), patch.object(p, "indexed") as index:
            with self.assertRaisesRegex(RuntimeError, "Upload failed"):
                p.publish_one("test-crate", "1.0.0", ["cargo"], 60, 1)
        index.assert_not_called()

    def test_lost_upload_response_recovers_from_registry_confirmation(self):
        with patch.object(p, "uploaded", side_effect=[False, True, True]), patch.object(p.subprocess, "run", return_value=SimpleNamespace(returncode=101)), patch.object(p, "indexed", return_value=True):
            p.publish_one("test-crate", "1.0.0", ["cargo"], 60, 1)

    def test_index_timeout_is_not_success(self):
        with patch.object(p, "uploaded", return_value=True), patch.object(p, "indexed", return_value=False), patch.object(p.time, "monotonic", side_effect=[0, 61]):
            with self.assertRaisesRegex(p.RegistryError, "Timed out"):
                p.publish_one("test-crate", "1.0.0", ["cargo"], 60, 1)

    def test_registry_api_failure_does_not_trigger_upload(self):
        with patch.object(p, "uploaded", side_effect=p.RegistryError("HTTP 503")), patch.object(p.subprocess, "run") as run:
            with self.assertRaises(p.RegistryError):
                p.publish_one("test-crate", "1.0.0", ["cargo"], 60, 1)
        run.assert_not_called()

    def test_sparse_index_requires_exact_unyanked_version(self):
        with patch.object(p, "read_url", return_value='{"vers":"1.0.0","yanked":true}\n{"vers":"1.0.1","yanked":false}'):
            self.assertFalse(p.indexed("test-crate", "1.0.0"))
            self.assertTrue(p.indexed("test-crate", "1.0.1"))


if __name__ == "__main__":
    unittest.main()
