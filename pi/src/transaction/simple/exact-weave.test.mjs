import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import {execFileSync} from 'node:child_process';
import {ExactCode} from './exact-code.mjs';
import {applyExact} from './exact-weave.mjs';
import {performWeaveEdit} from '../../tools/weave-edit.ts';
const semBin=process.env.SEM_TEST_BIN||'sem';
test('one exact batch edits disjoint same-file entities without recapture, preserving UTF-8 and EOLs',async()=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'exact-siblings-'));
 try {
  execFileSync('git',['init','-q',root]);
  const file=path.join(root,'a.ts');
  for(const eol of ['\n','\r\n']) {
   const source=['// π untouched','export function first() { return 1; }','',
     '// untouched separator','export function second() { return 2; }','// end',''].join(eol);
   await fs.writeFile(file,source);
   const exact=new ExactCode({semBin}),snap=await exact.capture(root,['a.ts']);
   const id=name=>exact.resolve(snap.revision,name).matches[0].id;
   const result=await applyExact(exact,root,snap.revision,[
    {id:id('first'),old:'return 1',new:'const label = "λ"; return label.length'},
    {id:id('second'),old:'return 2',new:'return 200'},
   ],{semBin});
   assert.equal(result.status,'applied',JSON.stringify(result));
   assert.equal(await fs.readFile(file,'utf8'),source.replace('return 1','const label = "λ"; return label.length').replace('return 2','return 200'));
   await assert.rejects(applyExact(exact,root,snap.revision,[{id:id('first'),old:'return 1',new:'return 3'}],{semBin}),/STALE_SNAPSHOT/);
   await fs.writeFile(file,source);
   const failed=await applyExact(exact,root,snap.revision,[
    {id:id('first'),content:'export function renamed() { return 9; }'},
    {id:id('second'),old:'return 2',new:'return 200'},
   ],{semBin});
   assert.equal(failed.status,'apply_failed');
   assert.equal(await fs.readFile(file,'utf8'),source);
   const a=exact.get(snap.revision).entities.find(e=>e.id===id('first'));
   const b=exact.get(snap.revision).entities.find(e=>e.id===id('second'));
   await assert.rejects(performWeaveEdit({atomic:true,claim:false,edits:[
    {file:'a.ts',entity:{name:'first'},op:'replace',content:'export function first() { return 100; }'},
   ]},{cwd:root,semBin,checkDependents:false,snapshotTargetsByEdit:[]}),/INVALID_SNAPSHOT_TARGETS/);
   assert.equal(await fs.readFile(file,'utf8'),source);
   const thrown=await performWeaveEdit({atomic:true,claim:false,edits:[
    {file:'a.ts',entity:{name:'second'},op:'replace',content:'export function second() { return 200; }'},
    {file:'a.ts',entity:{name:'first'},op:'replace',content:'export function first() { return 100; }'},
   ]},{cwd:root,semBin,checkDependents:false,snapshotTargetsByEdit:[
    {file,sha256:snap.files[0].sha256,start:b.start,end:b.end},
    {file,sha256:'intentionally-wrong-intermediate-hash',start:a.start,end:a.end},
   ]});
   assert.equal(thrown.details.rolledBack,true);
   assert.equal(await fs.readFile(file,'utf8'),source);
  }
 }finally{await fs.rm(root,{recursive:true,force:true});}
});

test('nested entities overlap and are rejected before mutation',async()=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'exact-overlap-'));
 try {
  const source='class Example {\n  method() { return 1; }\n}\n';
  await fs.writeFile(path.join(root,'a.ts'),source);
  const exact=new ExactCode({semBin}),snap=await exact.capture(root,['a.ts']);
  const parent=exact.resolve(snap.revision,'Example').matches[0];
  const child=exact.get(snap.revision).entities.find(e=>e.name==='method');
  await assert.rejects(applyExact(exact,root,snap.revision,[
   {id:parent.id,content:source},{id:child.id,old:'return 1',new:'return 2'}
  ],{semBin}),/OVERLAPPING_EDITS/);
  assert.equal(await fs.readFile(path.join(root,'a.ts'),'utf8'),source);
 }finally{await fs.rm(root,{recursive:true,force:true});}
});

test('snapshot IDs directly drive real Weave edits and reject stale source',async()=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'exact-weave-test-'));
 try {
  execFileSync('git',['init','-q',root]);
  const file=path.join(root,'a.ts'),source='export function hello() { return 1; }\n';
  await fs.writeFile(file,source);
  const exact=new ExactCode({semBin});const snap=await exact.capture(root,['a.ts']);
  const discovered=exact.query(snap.revision,[{contains:'return 1',view:'source'}]);
  assert.equal(discovered.results[0].status,'matched');
  const e=discovered.sources[0].entity;
  const edits=[{id:e.id,old:'return 1',new:'return 2'}];
  await assert.rejects(applyExact(exact,root,snap.revision,[{id:e.id,old:'absent',new:'x'}],{semBin}),/ANCHOR_NOT_UNIQUE/);
  await assert.rejects(applyExact(exact,root,snap.revision,[...edits,...edits],{semBin}),/OVERLAPPING_EDITS/);
  await fs.writeFile(file,source+'// concurrent\n');
  await assert.rejects(applyExact(exact,root,snap.revision,edits,{semBin}),/STALE_SNAPSHOT/);
  assert.match(await fs.readFile(file,'utf8'),/concurrent/);
  // Test the engine's queued guard independently of the bridge's preflight.
  await assert.rejects(performWeaveEdit({file:'a.ts',entity:{name:'hello'},op:'replace',content:'export function hello() { return 2; }',claim:false},
   {cwd:root,semBin,checkDependents:false,snapshotTargets:new Map([[file,{sha256:snap.files[0].sha256,start:e.start,end:e.end}]])}),/STALE_SNAPSHOT/);
  await fs.writeFile(file,source);
  let checks=0;
  const receipt=await applyExact(exact,root,snap.revision,edits,{semBin,validate:async()=>{checks++;return {pass:true};}});
  assert.equal(receipt.status,'applied');assert.equal(checks,1);
  assert.equal(receipt.validation.edited_files_unchanged,true);
  assert.match(await fs.readFile(file,'utf8'),/return 2/);
  await assert.rejects(applyExact(exact,root,snap.revision,edits,{semBin}),/STALE_SNAPSHOT/);
  assert.match(exact.read(snap.revision,e.id).content,/return 1/);
  const newer=await exact.capture(root,['a.ts']);const id=exact.resolve(newer.revision,'hello').matches[0].id;
  const changed=await applyExact(exact,root,newer.revision,[{id,content:'export function hello() { return 3; }'}],{semBin,validate:async()=>{
   await fs.appendFile(file,'// external\n');return {pass:true};
  }});
  assert.equal(changed.validation.edited_files_unchanged,false);
  assert.equal(changed.validation.pass,null);
  await fs.writeFile(file,source);
  await fs.writeFile(path.join(root,'b.ts'),'export function bye() { return 0; }\n');
  const both=await exact.capture(root,['a.ts','b.ts']);
  const first=exact.resolve(both.revision,'hello').matches[0].id;
  const second=exact.resolve(both.revision,'bye').matches[0].id;
  let checked=false;
  const rollback=await applyExact(exact,root,both.revision,[
    {id:first,content:'export function hello() { return 9; }'},
    {id:second,content:'export function renamed() { return 0; }'},
  ],{semBin,validate:async()=>{checked=true;return {pass:true};}});
  assert.equal(rollback.status,'apply_failed');assert.equal(checked,false);
  assert.equal(await fs.readFile(file,'utf8'),source);
  assert.equal(await fs.readFile(path.join(root,'b.ts'),'utf8'),'export function bye() { return 0; }\n');
 }finally{await fs.rm(root,{recursive:true,force:true});}
});
