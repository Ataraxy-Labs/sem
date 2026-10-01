import unittest

from evidence_focus import index


class GradleEvidenceTests(unittest.TestCase):
    def test_parameterized_failure_keeps_assertion_and_location(self):
        diagnostics, totals = index({'result': {
            'stdout': 'RenderTest > renders(boolean) > engine=true FAILED\n'
                      '    org.opentest4j.AssertionFailedError at RenderTest.java:72\n\n',
            'stderr': 'native.WatcherException: benign warning\n'
                      '30 tests completed, 10 failed\nBUILD FAILED in 43s\n'}})
        self.assertEqual(diagnostics[0]['text'],
                         'RenderTest > renders(boolean) > engine=true FAILED')
        self.assertIn('AssertionFailedError', diagnostics[0]['context_lines'][0])
        self.assertEqual(diagnostics[0]['location']['line'], 72)
        self.assertEqual(len(totals), 2)

    def test_plain_failed_word_is_not_a_gradle_test(self):
        diagnostics, _ = index({'result': {'stdout': 'request FAILED\n'}})
        self.assertEqual(diagnostics, [])


if __name__ == '__main__':
    unittest.main()
