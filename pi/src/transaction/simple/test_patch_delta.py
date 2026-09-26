import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from patch_delta import incremental_patch

class IncrementalPatchTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory(prefix='sem-delta-fixture-')
        self.root=Path(self.tmp.name)/'source'; self.root.mkdir()
        self.git('init','-q')
        for name, content in {'a':'old a\n','b':'old b\n','space name':'old spaced\n'}.items():
            (self.root/name).write_text(content)
        (self.root/'binary').write_bytes(b'\0initial\xff')
        self.git('add','.')
        self.git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-qm','base')
        self.checkout=Path(self.tmp.name)/'checker'
        self.git('worktree','add','--detach',str(self.checkout),'HEAD')
    def tearDown(self):self.tmp.cleanup()
    def git(self,*args,cwd=None,input=None):
        return subprocess.run(['git',*args],cwd=cwd or self.root,input=input,
            text=True,capture_output=True,check=True).stdout
    def patch(self):
        self.git('add','-N','.')
        return self.git('diff','--no-ext-diff','--no-textconv','--binary','HEAD')
    def apply(self,patch):
        if patch:self.git('apply','--binary','-',cwd=self.checkout,input=patch)
    def test_only_changed_file_is_touched(self):
        (self.root/'a').write_text('changed a\n')
        old=self.patch();self.apply(old)
        before=(self.checkout/'a').stat().st_mtime_ns
        (self.root/'b').write_text('changed b\n')
        new=self.patch();delta=incremental_patch(self.root,old,new)
        self.assertNotIn('diff --git a/a b/a',delta)
        self.apply(delta)
        self.assertEqual((self.checkout/'a').stat().st_mtime_ns,before)
        self.assertEqual((self.checkout/'b').read_bytes(),(self.root/'b').read_bytes())
        self.assertEqual(incremental_patch(self.root,new,new),'')
    def test_binary_rename_create_and_delete(self):
        (self.root/'created').write_text('created\n')
        (self.root/'binary').write_bytes(b'\0first\xff')
        old=self.patch();self.apply(old)
        (self.root/'binary').write_bytes(b'\0second\xfe')
        (self.root/'space name').rename(self.root/'renamed space')
        (self.root/'created').unlink()
        new=self.patch();self.apply(incremental_patch(self.root,old,new))
        self.assertFalse((self.checkout/'created').exists())
        self.assertFalse((self.checkout/'space name').exists())
        self.assertEqual((self.checkout/'renamed space').read_bytes(),(self.root/'renamed space').read_bytes())
        self.assertEqual((self.checkout/'binary').read_bytes(),(self.root/'binary').read_bytes())
    def test_bad_previous_patch_fails_without_worktree_changes(self):
        original=(self.root/'a').read_bytes()
        with self.assertRaises(subprocess.CalledProcessError):
            incremental_patch(self.root,'not a patch','')
        self.assertEqual((self.root/'a').read_bytes(),original)
    def test_return_to_base(self):
        (self.root/'a').write_text('changed\n');old=self.patch();self.apply(old)
        self.apply(incremental_patch(self.root,old,''))
        self.assertEqual(self.git('diff','--no-ext-diff','--no-textconv','HEAD',cwd=self.checkout),'')
    def test_real_index_is_unchanged(self):
        (self.root/'a').write_text('changed\n');old=self.patch()
        index=self.root/'.git/index';before=index.read_bytes()
        incremental_patch(self.root,'',old)
        self.assertEqual(index.read_bytes(),before)

if __name__=='__main__':unittest.main()
