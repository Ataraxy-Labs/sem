import importlib.util
from pathlib import Path
import unittest
spec=importlib.util.spec_from_file_location('resources',Path(__file__).with_name('validation_resources.py'))
resources=importlib.util.module_from_spec(spec);spec.loader.exec_module(resources)

class ResourceTests(unittest.TestCase):
    def test_bazel_wrappers_keep_targets_and_test_args(self):
        for prefix in [['bazel'],['bazelisk'],['yarn','bazel'],['yarn','run','bazel'],['npm','run','bazel']]:
            result=resources.bounded_command(prefix+['test','--keep_going','//a:test','//b:test','--','test_arg'])
            self.assertEqual(result[:len(prefix)],prefix)
            self.assertEqual(result[len(prefix)],'--host_jvm_args=-Xmx1024m')
            self.assertEqual(result[-5:],['--keep_going','//a:test','//b:test','--','test_arg'])
            self.assertIn('--jobs=2',result)
    def test_non_bazel_commands_unchanged(self):
        for argv in [['cargo','test','--offline'],['yarn','test'],['bazel','version']]:
            self.assertEqual(resources.bounded_command(argv),argv)
    def test_conflicting_resource_flags_are_rejected(self):
        for flag in ['--jobs=10','--local_test_jobs=8','--host_jvm_args=-Xmx8g']:
            with self.assertRaises(ValueError):resources.bounded_command(['bazel','test',flag,'//a:test'])

if __name__=='__main__':unittest.main()
