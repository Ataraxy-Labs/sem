import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {selectEntity,invalidateChanged,acknowledgeDefinitions} from './repair-contract.mjs';
test('typed selectors never collapse impl into same-name struct',()=>{
  const entries=[{name:'BuildConfig',type:'struct',start_line:1},{name:'BuildConfig',type:'impl',start_line:10}];
  assert.equal(selectEntity(entries,{name:'BuildConfig',entity_type:'impl'}).start_line,10);
  assert.throws(()=>selectEntity(entries,{name:'BuildConfig'}),/Ambiguous/);
  assert.throws(()=>selectEntity(entries,{name:'BuildConfig',entity_type:'class'}),/incompatible/);
  assert.throws(()=>selectEntity(entries,{name:'BuildConfig',entity_type:'impl',ordinal:1}),/incompatible/);
});
test('duplicate typed entities require a valid ordinal or parent',()=>{
  const entries=[{name:'new',type:'method',parent_name:'A'},{name:'new',type:'method',parent_name:'B'}];
  assert.equal(selectEntity(entries,{name:'new',ordinal:1}).parent_name,'B');
  assert.equal(selectEntity(entries,{name:'NEW',parent_name:'a'}).parent_name,'A');
});
test('mutations invalidate changed files but preserve unrelated program context',async()=>{
  const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-cache-contract-'));
  try {
    await fs.writeFile(path.join(cwd,'a'),'old');await fs.writeFile(path.join(cwd,'b'),'stable');
    const hash=s=>createHash('sha256').update(s).digest('hex');
    const snapshots=new Map([['a',hash('old')],['b',hash('stable')]]);
    const entities=new Map([['A',{file:'a'}],['B',{file:'b'}]]),files=new Map([['a',{}],['b',{}]]);
    await fs.writeFile(path.join(cwd,'a'),'new');
    assert.deepEqual(await invalidateChanged(cwd,snapshots,entities,files),['a']);
    assert.deepEqual([...entities.keys()],['B']);assert.deepEqual([...snapshots.keys()],['b']);
    assert.deepEqual(await invalidateChanged(cwd,snapshots,entities,files),[]);
  } finally {await fs.rm(cwd,{recursive:true,force:true});}
});
test('only explicitly acknowledged identical complete definitions omit their bodies',()=>{
  const d={file:'a',entity:{name:'f'},content:'body'};
  const first=acknowledgeDefinitions([d])[0];
  assert.equal(acknowledgeDefinitions([d])[0].content,'body');
  assert.equal(acknowledgeDefinitions([d],[first.receipt])[0].reused,true);
  assert.equal(acknowledgeDefinitions([{...d,content:'changed'}],[first.receipt])[0].content,'changed');
  const truncated={...d,truncated:true};const receipt=acknowledgeDefinitions([truncated])[0].receipt;
  assert.equal(acknowledgeDefinitions([truncated],[receipt])[0].content,'body');
});
