"""Compute a binary-safe incremental patch without touching the source worktree."""
import os
from pathlib import Path
import subprocess
import tempfile

def incremental_patch(repository, previous, current):
    if previous == current:
        return ''
    with tempfile.TemporaryDirectory(prefix='sem-check-index-') as tmp:
        environment={**os.environ, 'GIT_INDEX_FILE':str(Path(tmp)/'index')}
        def git(*args, input=None):
            return subprocess.run(['git',*args],cwd=repository,env=environment,
                input=input,capture_output=True,text=True,check=True,timeout=60).stdout
        trees=[]
        for patch in (previous,current):
            git('read-tree','HEAD')
            if patch:
                git('apply','--cached','--binary','--whitespace=nowarn','-',input=patch)
            trees.append(git('write-tree').strip())
        return git('diff','--no-ext-diff','--no-textconv','--binary',*trees,'--')
