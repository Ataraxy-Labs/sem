import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {normalizeEdits} from './normalize-edits-simple.mjs';
test('ambiguous anchors report the actual entity scope without weakening uniqueness',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-anchor-error-'));
 const source='class Callback:\n def start(self):\n  """call method; inputs to call method"""\n';
 try {
  await fs.writeFile(path.join(cwd,'a.py'),source);
  const api={outline:async()=>({entities:[{name:'Callback',type:'class',start_line:1,end_line:3},
   {name:'start',type:'function',parent_name:'Callback',start_line:2,end_line:3}]})};
  await assert.rejects(normalizeEdits([{file:'a.py',entity:{name:'start',parent_name:'Callback'},old:'call method',new:'async call'}],cwd,api),
   /scope: entity Callback.start .*multiple exact matches/);
  await assert.rejects(normalizeEdits([{file:'a.py',entity:{name:'start',parent_name:'Callback'},old:'missing phrase',new:'async call'}],cwd,api),
   /scope: entity Callback.start .*no unique exact or whitespace-flexible match/);
  assert.equal(await fs.readFile(path.join(cwd,'a.py'),'utf8'),source);
 } finally {await fs.rm(cwd,{recursive:true});}
});
test('scoped anchors stay scoped when composed into a parent with repeated text',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-scope-'));
 const source='impl Foo {\n fn a(mode: Old) {}\n fn b(mode: Old) {}\n}\n';
 const entities=[{name:'Foo',type:'impl',start_line:1,end_line:4},
  {name:'a',type:'function',parent_name:'Foo',start_line:2,end_line:2},
  {name:'b',type:'function',parent_name:'Foo',start_line:3,end_line:3}];
 const api={outline:async()=>({entities})};
 try {
  await fs.writeFile(path.join(cwd,'a.rs'),source);
  const edits=['a','b'].map(name=>({file:'a.rs',entity:{name,parent_name:'Foo'},old:'mode: Old',new:'intent: UserIntent'}));
  for(const batch of [edits,[...edits].reverse()]) {
   const r=await normalizeEdits(batch,cwd,api);
   assert.equal(r.edits.length,1);
   assert.equal(r.edits[0].content,source.trimEnd().replaceAll('mode: Old','intent: UserIntent'));
  }
  await assert.rejects(normalizeEdits([edits[0],edits[0]],cwd,api),/CONFLICTING_TEXT_EDITS/);
  await assert.rejects(normalizeEdits([{file:'a.rs',old:'mode: Old',new:'x'}],cwd,api),/exactly once/);
  assert.equal(await fs.readFile(path.join(cwd,'a.rs'),'utf8'),source);
 }finally{await fs.rm(cwd,{recursive:true});}
});
