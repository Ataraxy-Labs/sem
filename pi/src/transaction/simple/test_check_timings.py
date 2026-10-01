import importlib.util
import os
import subprocess
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('check_timings',Path(__file__).with_name('container-check-v8.py'))
checker=importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

class TimingTests(unittest.TestCase):
    def test_timeout_preserves_output_and_restores_checker(self):
        calls=[]
        def invoke(argv,**kwargs):
            calls.append(argv)
            if 'env' in argv:
                raise subprocess.TimeoutExpired(argv,600,output=b'Running TestSlow\n',stderr=b'partial warning')
            stdout=''
            if argv[:2]==['git','diff'] or '/tmp/sem-applied.diff' in argv: stdout='same patch'
            elif argv[:3]==['docker','inspect','--format']: stdout='{"Running":true}'
            return SimpleNamespace(returncode=0,stdout=stdout,stderr='')
        env={'SEM_VALIDATION_CONTAINER':'fixture','SEM_VALIDATION_IMAGE':'image',
             'SEM_VALIDATION_CWD':'/tmp','SEM_VALIDATION_BASE':'base'}
        with patch.dict(os.environ,env),patch.object(checker.subprocess,'run',side_effect=invoke):
            result=checker._check('go test ./...')
        self.assertIsNone(result['pass'])
        self.assertEqual(result['stage'],'timeout')
        self.assertEqual(result['stdout'],'Running TestSlow\n')
        self.assertEqual(result['stderr'],'partial warning')
        self.assertIn(['docker','restart','-t','1','fixture'],calls)
        self.assertIn('checker_restarted',result['recovery'])

    def test_timings_preserve_command_output_and_verdict(self):
        for exit_code in [0,1]:
            calls=[]
            def invoke(argv,**kwargs):
                calls.append(argv)
                stdout=''
                rc=0
                if argv[:2]==['git','diff'] or '/tmp/sem-applied.diff' in argv:
                    stdout='same patch'
                elif argv[:3]==['docker','inspect','--format']:
                    stdout='{"OOMKilled":false}'
                elif 'env' in argv:
                    stdout='test diagnostics';rc=exit_code
                return SimpleNamespace(returncode=rc,stdout=stdout,stderr='')
            env={'SEM_VALIDATION_CONTAINER':'fixture','SEM_VALIDATION_IMAGE':'image',
                 'SEM_VALIDATION_CWD':'/tmp','SEM_VALIDATION_BASE':'base'}
            with patch.dict(os.environ,env),patch.object(checker.subprocess,'run',side_effect=invoke):
                result=checker._check('go test ./...')
            self.assertEqual(result['pass'],exit_code==0)
            self.assertEqual(result['stdout'],'test diagnostics')
            self.assertEqual(result['executed_argv'],['go','test','./...'])
            self.assertTrue(result['patch_unchanged'])
            self.assertEqual(set(result['timings_ms']),{
                'container_ready_ms','patch_sync_ms','command_ms','state_inspection_ms'})
            self.assertTrue(all(value>=0 for value in result['timings_ms'].values()))
            self.assertEqual(sum('env' in call for call in calls),1)

if __name__=='__main__':unittest.main()
