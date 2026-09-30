import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {ExactCode} from './exact-code.mjs';

test('read batches preserve valid siblings without following symlinks or weakening capture',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-partial-read-'));
  try {
    const content='def value():\n    return 1\n';
    await fs.writeFile(path.join(root,'good.py'),content);
    await fs.symlink('good.py',path.join(root,'alias.py'));
    const exact=new ExactCode({semBin:process.env.SEM_TEST_BIN||'sem'});
    const captured=await exact.capture(root,['good.py','alias.py'],{partialReads:true});
    assert.equal(captured.complete,false);
    const selectors=[{file:'alias.py'},{file:'good.py'},null,{name:'value'}];
    const result=exact.query(captured.revision,selectors);
    assert.deepEqual(result.results.map(r=>r.status),['error','unique','error','unique']);
    assert.equal(result.results[0].error.code,'SYMLINK_NOT_SUPPORTED');
    assert.equal(result.results[2].error.code,'INVALID_SELECTOR');
    assert.equal(result.files[0].content,content);
    assert.equal(result.sources[0].entity.name,'value');
    assert.equal(result.complete,false);
    assert.equal(result.file_errors.length,1);
    assert.deepEqual(exact.query(captured.revision,selectors),result);
    await assert.rejects(exact.capture(root,['good.py','alias.py']),/SYMLINK_NOT_SUPPORTED/);
    await assert.rejects(exact.capture(root,['../outside'],{partialReads:true}),/INVALID_PATH/);
    const complete=await exact.capture(root,['good.py']);
    assert.notEqual(complete.revision,captured.revision);
    assert.throws(()=>exact.query(complete.revision,[]),/INVALID_SELECTORS/);
  } finally {await fs.rm(root,{recursive:true,force:true});}
});
