import unittest

from evidence_focus import index


class JestEvidenceTests(unittest.TestCase):
    def test_named_assertions_include_values_and_source(self):
        diagnostics, _ = index({'result': {'stderr':
            'FAIL components/button.test.tsx\n'
            '  ● Button › hides content\n\n'
            '    expect(received).toBe(expected)\n\n'
            '    Expected: false\n    Received: true\n\n'
            '      at toBe (components/button.test.tsx:197:50)\n\n'
            '  ● Button › preserves state\n\n'
            '    Expected: 2\n    Received: 1\n\n'
            'Tests: 2 failed, 100 passed, 102 total\n'}})
        failures = [d for d in diagnostics if d['text'].startswith('●')]
        self.assertEqual(len(failures), 2)
        self.assertIn('Expected: false', failures[0]['context_lines'])
        self.assertIn('Received: true', failures[0]['context_lines'])
        self.assertEqual(failures[0]['location']['line'], 197)
        self.assertNotIn('Expected: 2', failures[0]['context_lines'])

    def test_assertion_context_is_bounded(self):
        diagnostics, _ = index({'result': {'stderr':
            '  ● long diff\n' + '    value\n' * 200}})
        self.assertEqual(len(diagnostics[0]['context_lines']), 24)
        self.assertTrue(diagnostics[0]['context_truncated'])


if __name__ == '__main__':
    unittest.main()
