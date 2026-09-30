import importlib.util
import os
from pathlib import Path
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location('checker',Path(__file__).with_name('container-check-v8.py'))
checker=importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

class RecoveryTests(unittest.TestCase):
    def test_timeout_preserves_diagnostics_and_retry_works(self):
        for restart_fails in (False,True):
            calls=[]
            def invoke(argv,**kwargs):
                calls.append(argv)
                if 'env' in argv:
                    raise subprocess.TimeoutExpired(argv,600,output=b'Running TestSlow',stderr=b'warning')
                if argv[:2]==['docker','restart'] and restart_fails:
                    raise subprocess.CalledProcessError(1,argv)
                stdout=''
                if argv[:2]==['git','diff'] or '/tmp/sem-applied.diff' in argv: stdout='same patch'
                elif argv[:3]==['docker','inspect','--format']: stdout='{"Running":false}'
                return SimpleNamespace(returncode=0,stdout=stdout,stderr='')
            env={'SEM_VALIDATION_CONTAINER':'fixture','SEM_VALIDATION_IMAGE':'image',
                 'SEM_VALIDATION_CWD':'/tmp','SEM_VALIDATION_BASE':'base'}
            with patch.dict(os.environ,env),patch.object(checker.subprocess,'run',side_effect=invoke):
                result=checker._check('go test ./...')
            self.assertIsNone(result['pass'])
            self.assertEqual(result['stdout'],'Running TestSlow')
            self.assertEqual(result['stderr'],'warning')
            self.assertIn(['docker','start','fixture'],calls)
            self.assertIn(['docker','restart','-t','1','fixture'],calls)
            self.assertIn('checker_restart_failed' if restart_fails else 'checker_restarted',result['recovery'])
