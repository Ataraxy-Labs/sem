import io
import tarfile
import unittest
from unittest.mock import Mock
from junit_reports import parse_archive, enrich_failed_check

class EnrichmentTests(unittest.TestCase):
    def test_success_unavailable_and_other_runners_do_not_collect(self):
        collector=Mock()
        for passed,stage,argv in [(True,'test',['./gradlew','test']),
                                 (None,'timeout',['mvn','test']),
                                 (False,'test',['pytest'])]:
            result={'pass':passed,'stage':stage,'executed_argv':argv}
            self.assertIs(enrich_failed_check(result,'container',100,collector),result)
        collector.assert_not_called()

    def test_failure_details_do_not_change_verdict_or_provenance(self):
        original={'pass':False,'stage':'test','executed_argv':['./gradlew','test'],
                  'exit_code':1,'checked_patch_sha256':'patch',
                  'timings_ms':{'command_ms':500},'stdout':'original'}
        collector=Mock(return_value={'status':'available','failures':[]})
        enriched=enrich_failed_check(original,'container',100,collector)
        collector.assert_called_once_with('container','gradle',100)
        for key in ('pass','stage','exit_code','checked_patch_sha256','stdout'):
            self.assertEqual(enriched[key],original[key])
        self.assertEqual(enriched['timings_ms']['command_ms'],500)
        self.assertNotIn('test_report_evidence',original)
        self.assertIn('report_collection_ms',enriched['timings_ms'])

    def test_report_failure_does_not_hide_test_failure(self):
        result={'pass':False,'stage':'test','executed_argv':['mvn','test'],'exit_code':1}
        enriched=enrich_failed_check(result,'container',100,Mock(side_effect=RuntimeError('broken')))
        self.assertFalse(enriched['pass'])
        self.assertEqual(enriched['exit_code'],1)
        self.assertEqual(enriched['test_report_evidence']['status'],'unavailable')

class ArchiveTests(unittest.TestCase):
    def archive(self,entries):
        buffer=io.BytesIO()
        with tarfile.open(fileobj=buffer,mode='w') as archive:
            for name,mtime,content,kind in entries:
                info=tarfile.TarInfo(name);info.mtime=mtime;info.type=kind
                info.size=len(content) if kind==tarfile.REGTYPE else 0
                archive.addfile(info,io.BytesIO(content))
        return buffer.getvalue()

    def test_fresh_reports_only_without_extracting_links(self):
        xml=b'<testsuite><testcase name="case"><failure message="mismatch"/></testcase></testsuite>'
        payload=self.archive([('new.xml',100,xml,tarfile.REGTYPE),
                              ('old.xml',20,xml,tarfile.REGTYPE),
                              ('link.xml',100,b'',tarfile.SYMTYPE)])
        result=parse_archive(payload,99)
        self.assertEqual(result['reports_read'],1)
        self.assertEqual(result['reports_skipped'],1)
        self.assertEqual(result['failures'][0]['message'],'mismatch')
        self.assertNotIn('pass',result)

    def test_bad_xml_does_not_erase_other_evidence(self):
        payload=self.archive([('bad.xml',100,b'<broken',tarfile.REGTYPE),
                              ('good.xml',100,b'<testsuite/>',tarfile.REGTYPE)])
        result=parse_archive(payload,99)
        self.assertEqual(result['reports_read'],1)
        self.assertEqual(result['reports_skipped'],1)

if __name__=='__main__':unittest.main()
