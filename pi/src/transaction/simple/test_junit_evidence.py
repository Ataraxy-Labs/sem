import unittest
from junit_evidence import parse_report, assertion_difference

class JunitTests(unittest.TestCase):
    def test_long_assertion_locates_difference_outside_prefix(self):
        common='line\n'*1000
        result=assertion_difference('expected: <'+common+'red> but was: <'+common+'blue>')
        self.assertEqual(result['first_difference_character'],len(common))
        self.assertTrue(result['expected'].endswith('red'))
        self.assertTrue(result['actual'].endswith('blue'))
        self.assertLess(len(result['expected']),400)
        self.assertIsNone(assertion_difference('an unfamiliar assertion'))
        self.assertIsNone(assertion_difference('expected: <x> but was: <y> but was: <z>'))

    def test_failure_message_and_trace_without_rerunning(self):
        result=parse_report(b'''<testsuite><testcase name="style(true)" classname="StyleTest">
          <failure message="expected: red but was: blue">at StyleTest.java:96</failure>
          </testcase></testsuite>''','TEST-StyleTest.xml')
        self.assertEqual(result['failures'][0]['message'],'expected: red but was: blue')
        self.assertIn('StyleTest.java:96',result['failures'][0]['detail'])
        self.assertNotIn('pass',result)

    def test_bounded_and_incomplete_reports_are_explicit(self):
        xml=b'<testsuites>'+b''.join(('<testcase><error message="oops'+str(i)+'"/></testcase>').encode() for i in range(3))+b'</testsuites>'
        result=parse_report(xml,'test.xml',limit=1)
        self.assertEqual(result['omitted_failures'],2)
        self.assertEqual(result['total_failure_elements'],3)

    def test_identical_parameterized_failures_keep_counts_and_test_identity(self):
        xml=b'<testsuite>'+b''.join(('<testcase name="case'+str(i)+'"><failure message="same">same trace</failure></testcase>').encode() for i in range(7))+b'</testsuite>'
        result=parse_report(xml,'test.xml',limit=1)
        self.assertEqual(result['omitted_failures'],0)
        self.assertEqual(result['failures'][0]['occurrences'],7)
        self.assertEqual(result['failures'][0]['omitted_test_names'],2)
        self.assertEqual(len(result['failures'][0]['tests']),5)

    def test_success_report_never_becomes_a_verdict(self):
        result=parse_report(b'<testsuite failures="0"/>','test.xml')
        self.assertEqual(result['failures'],[])
        self.assertNotIn('pass',result)

    def test_rejects_oversized_and_entity_payloads(self):
        for payload in [b'x'*(2*1024*1024+1),
                        b'<!DOCTYPE x [<!ENTITY x "value">]><testsuite/>']:
            with self.assertRaises(ValueError):parse_report(payload,'test.xml')

    def test_large_diagnostics_are_marked_truncated(self):
        result=parse_report(b'<testcase><failure>'+b'x'*2600+b'</failure></testcase>','test.xml')
        self.assertTrue(result['failures'][0]['truncated'])
        self.assertEqual(len(result['failures'][0]['detail']),2500)

if __name__=='__main__':unittest.main()
