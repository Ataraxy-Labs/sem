import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('checker_cpp', Path(__file__).with_name('container-check-v8.py'))
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

class CppValidation(unittest.TestCase):
    def test_cmake_and_ctest_reach_resource_policy(self):
        # Stop before Docker; prove plain C++ commands pass the shared gate.
        for command, expected in [('cmake --build build', ['cmake', '--build', 'build']),
                                  ('ctest --test-dir build', ['ctest', '--test-dir', 'build'])]:
            with patch.object(checker, 'bounded_command', side_effect=ValueError('policy reached')) as policy:
                self.assertEqual(checker._check(command)['error'], 'policy reached')
                policy.assert_called_once_with(expected)

    def test_rejections_advertise_capabilities_without_execution(self):
        for command in ['cmake --build build && ctest', 'unknown-command', 'ctest | head']:
            with patch.object(checker.subprocess, 'run') as run:
                result = checker._check(command)
                self.assertIsNone(result['pass'])
                self.assertIn('cmake', result['supported_commands'])
                self.assertIn('ctest', result['supported_commands'])
                run.assert_not_called()
