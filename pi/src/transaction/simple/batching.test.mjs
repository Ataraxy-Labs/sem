import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {readBoundedFiles} from './program-files.mjs';
import {normalizeEdits} from './normalize-edits-simple.mjs';

test('large file batches retain shared byte limit and explicit truncation',async()=>{
  const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-batch-read-'));
  try {
    const files=Array.from({length:16},(_,i)=>`${i}.ts`);
    await Promise.all(files.map(f=>fs.writeFile(path.join(cwd,f),'const a = 1;\n')));
    const full=await readBoundedFiles(cwd,files);
    assert.equal(full.length,16);
    assert.ok(full.every(f=>!f.truncated));
    const bounded=await readBoundedFiles(cwd,files,26);
    assert.equal(bounded.reduce((sum,f)=>sum+Buffer.byteLength(f.content),0),26);
    assert.equal(bounded.filter(f=>f.truncated).length,14);
    await assert.rejects(readBoundedFiles(cwd,Array(65).fill('0.ts')),/64/);
  } finally { await fs.rm(cwd,{recursive:true}); }
});

test('cross-file edit batch composes per file with one outline each and no normalization writes',async()=>{
  const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-batch-edit-'));
  const source='function f() { return oldA + oldB; }\n';
  const counts=new Map();
  const api={outline:async file=>{
    counts.set(file,(counts.get(file)||0)+1);
    return {entities:[{name:'f',type:'function',start_line:1,end_line:1}]};
  }};
  try {
    await Promise.all(['a.ts','b.ts'].map(f=>fs.writeFile(path.join(cwd,f),source)));
    const edits=['a.ts','b.ts'].flatMap(file=>['A','B'].map(s=>({file,old:`old${s}`,new:`new${s}`})));
    const result=await normalizeEdits(edits,cwd,api);
    assert.equal(result.edits.length,2);
    assert.ok(result.edits.every(e=>e.content===source.trimEnd().replaceAll('old','new')));
    assert.deepEqual([...counts.values()],[1,1]);
    assert.equal(await fs.readFile(path.join(cwd,'a.ts'),'utf8'),source);
    await assert.rejects(normalizeEdits([edits[0],edits[0]],cwd,api),/CONFLICTING_TEXT_EDITS/);
  } finally { await fs.rm(cwd,{recursive:true}); }
});
