import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {ExactCode} from './exact-code.mjs';
import {EntityInterface} from './entity-interface.mjs';

test('read batches retain valid source, expose rejected aliases, and keep strict capture',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-partial-capture-'));
  try {
    const content='export function value() { return 1; }\n';
    await fs.writeFile(path.join(root,'good.ts'),content);
    await fs.symlink('good.ts',path.join(root,'alias.ts'));
    const exact=new ExactCode();
    // Real filesystem capture; parser stub isolates the path contract.
    exact.parseEntityRows=async()=>[];
    const api=await new EntityInterface(root,{exact}).initialize();
    const result=await api.query({files:['alias.ts','good.ts'],selectors:[{file:'alias.ts'},{file:'good.ts'}]});
    assert.equal(result.complete,false);
    assert.equal(result.results[0].error.code,'SYMLINK_NOT_SUPPORTED');
    assert.equal(result.results[1].status,'unique');
    assert.equal(result.files[0].content,content);
    assert.equal(result.file_errors.length,1);
    const again=await api.query({snapshot:result.snapshot,selectors:[{file:'alias.ts'},{file:'good.ts'}]});
    assert.deepEqual(again.results,result.results);
    const global=exact.query(result.snapshot,[{name:'absent'}]);
    assert.equal(global.complete,false);
    assert.equal(global.file_errors.length,1);
    await assert.rejects(exact.capture(root,['alias.ts','good.ts']),/SYMLINK_NOT_SUPPORTED/);
    const valid=await exact.capture(root,['good.ts']);
    assert.notEqual(valid.revision,result.snapshot);
    await assert.rejects(exact.capture(root,['../outside'],{partialReads:true}),/INVALID_PATH/);
    // Range recovery remains per item and never silently clamps source.
    const ranges=exact.query(valid.revision,[{file:'good.ts',start_byte:0,end_byte:10000},{file:'good.ts'}]);
    assert.equal(ranges.results[0].error.code,'INVALID_SOURCE_RANGE');
    assert.equal(ranges.files[0].content,content);
  } finally { await fs.rm(root,{recursive:true,force:true}); }
});
