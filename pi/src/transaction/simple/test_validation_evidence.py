import json
from pathlib import Path
import tempfile
import unittest
from validation_evidence import present, read


class EvidenceTests(unittest.TestCase):
    def test_success_is_smaller_and_exactly_recoverable(self):
        with tempfile.TemporaryDirectory() as directory:
            result = {'stage': 'test', 'pass': True, 'exit_code': 0,
                      'stdout': 'build progress 😀\n' * 2000 + 'warning: inspect this\ntest result: ok',
                      'stderr': '', 'executed_argv': ['cargo', 'test']}
            compact = present(result, directory, {'patch': 'abc'})
            self.assertLess(len(json.dumps(compact)), len(json.dumps(result)))
            self.assertIn('warning: inspect this', compact['stdout'])
            command, chunks = compact['evidence']['read_command'], []
            while command:
                part = read(command, directory)
                self.assertIsNone(part['pass'])
                chunks.append(part['content'])
                command = part['next_command']
            self.assertEqual(json.loads(''.join(chunks))['result'], result)

    def test_failures_are_bounded_and_exactly_recoverable(self):
        with tempfile.TemporaryDirectory() as directory:
            result = {'stage': 'test', 'pass': False, 'exit_code': 1,
                      'stdout': 'E AssertionError: unexpected 😀\n' * 4000 + '\nFAILED tests/a.py::test_a\n11 failed, 5 passed',
                      'stderr': 'Error: another failure\n' * 4000}
            compact = present(result, directory, {'patch': 'abc'})
            self.assertFalse(compact['pass'])
            self.assertEqual(compact['exit_code'], 1)
            self.assertLessEqual(len(compact['stdout']), 3000)
            self.assertLessEqual(len(compact['stderr']), 3000)
            self.assertFalse(compact['diagnostics']['complete'])
            self.assertIn('11 failed, 5 passed', compact['stdout'])
            command, chunks = compact['evidence']['read_command'], []
            while command:
                part = read(command, directory)
                chunks.append(part['content'])
                command = part['next_command']
            self.assertEqual(json.loads(''.join(chunks))['result'], result)

    def test_unknown_is_not_compacted(self):
        with tempfile.TemporaryDirectory() as directory:
            for passed in (None,):
                result = {'stage': 'test', 'pass': passed, 'exit_code': 1, 'stdout': 'failure\n' * 4000}
                self.assertEqual(present(result, directory, {})['stdout'], result['stdout'])
            unavailable = {'stage': 'timeout', 'pass': None}
            self.assertEqual(present(unavailable, directory, {}), unavailable)

    def test_content_identity_and_corruption(self):
        with tempfile.TemporaryDirectory() as directory:
            result = {'stage': 'test', 'pass': True, 'exit_code': 0}
            a = present(result, directory, {'patch': 'a'})
            self.assertEqual(a, present(result, directory, {'patch': 'a'}))
            b = present(result, directory, {'patch': 'b'})
            self.assertNotEqual(a['evidence']['id'], b['evidence']['id'])
            (Path(directory) / (a['evidence']['id'] + '.json')).write_text('corrupted')
            with self.assertRaises(ValueError):
                read(a['evidence']['read_command'], directory)

    def test_traversal_rejected(self):
        for command in ('evidence:../../secret:0', 'evidence:' + 'a'*64 + ':-1'):
            with self.assertRaises(ValueError):
                read(command, '.')


if __name__ == '__main__':
    unittest.main()
