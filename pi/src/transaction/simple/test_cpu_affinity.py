import importlib.util
from pathlib import Path
from unittest import TestCase, mock
import os

spec=importlib.util.spec_from_file_location('checker',Path(__file__).with_name('container-check-v8.py'))
checker=importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

class CpuAffinityTests(TestCase):
    def test_hostname_is_short_stable_and_locally_resolvable(self):
        flags=checker.offline_hostname_flags('a'*200)
        hostname=flags[1]
        self.assertLess(len(hostname),64)
        self.assertEqual(flags,checker.offline_hostname_flags('a'*200))
        self.assertIn(hostname+':127.0.0.1',flags)
        self.assertIn('HOSTNAME='+hostname,flags)
        self.assertNotEqual(flags,checker.offline_hostname_flags('b'*200))
        self.assertNotIn('--network',flags)
    def test_default_unchanged(self):
        with mock.patch.dict(os.environ,{'SEM_VALIDATION_CPUSET':''}):
            self.assertEqual(checker.cpu_affinity_flags(),[])
    def test_explicit_affinity(self):
        for value in ['0','0-2','0,2']:
            with mock.patch.dict(os.environ,{'SEM_VALIDATION_CPUSET':value}):
                self.assertEqual(checker.cpu_affinity_flags(),['--cpuset-cpus',value])
    def test_invalid_rejected(self):
        with mock.patch.dict(os.environ,{'SEM_VALIDATION_CPUSET':'0 --privileged'}):
            with self.assertRaises(ValueError): checker.cpu_affinity_flags()
