import json
import tempfile
import unittest
from pathlib import Path
from validation_evidence import present


class ReportPresentationTests(unittest.TestCase):
    def test_full_report_retained_but_response_omits_repeated_stack_details(self):
        failure = {'message': 'x' * 1500, 'detail': 'stack frame\n' * 200,
                   'test': 'case', 'assertion_difference': {'expected': 'a', 'actual': 'b'}}
        result = {'stage': 'test', 'pass': False, 'exit_code': 1,
                  'stdout': '', 'stderr': '',
                  'test_report_evidence': {'status': 'available', 'failures': [failure]}}
        with tempfile.TemporaryDirectory() as directory:
            output = present(result, directory, {'patch_sha256': 'fixture'})
            excerpt = output['test_report_evidence']['failures'][0]
            self.assertEqual(len(excerpt['message']), 600)
            self.assertTrue(excerpt['message_truncated'])
            self.assertTrue(excerpt['detail_omitted'])
            self.assertNotIn('detail', excerpt)
            self.assertEqual(excerpt['assertion_difference'], failure['assertion_difference'])
            record = json.loads((Path(directory) / (output['evidence']['id'] + '.json')).read_text())
            self.assertEqual(record['result'], result)
            self.assertFalse(output['pass'])
            self.assertEqual(result['test_report_evidence']['failures'][0], failure)
            self.assertLess(len(json.dumps(output)), len(json.dumps(result)))


if __name__ == '__main__':
    unittest.main()
