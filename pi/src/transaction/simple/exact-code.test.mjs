import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {ExactCode} from './exact-code.mjs';
const semBin=process.env.SEM_TEST_BIN || 'sem';
test('scoped Python methods and missing files resolve without a recovery call',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-scoped-test-'));
  try {
    await fs.writeFile(path.join(root,'a.py'),'class A:\n    def run(self):\n        return 1\n\nclass B:\n    def run(self):\n        return 2\n');
    const api=new ExactCode({semBin});
    await assert.rejects(api.capture(root,['AGENTS.md','a.py']),/ENOENT/);
    const s=await api.capture(root,['AGENTS.md','a.py'],{allowMissing:true});
    assert.deepEqual(s.missing_files,['AGENTS.md']);
    const result=api.query(s.revision,[{file:'AGENTS.md'},{file:'a.py',name:'A.run'},{name:'run'}]);
    assert.deepEqual(result.results.map(r=>r.status),['not_found','unique','ambiguous']);
    assert.match(result.sources[0].content,/return 1/);
    await assert.rejects(api.capture(root,['../escape.py'],{allowMissing:true}),/INVALID_PATH/);
    await fs.symlink(path.join(root,'a.py'),path.join(root,'link.py'));
    await assert.rejects(api.capture(root,['link.py'],{allowMissing:true}),/SYMLINK/);
  } finally {await fs.rm(root,{recursive:true,force:true});}
});
test('whole-file reads defer oversized files explicitly and share a byte budget',()=>{
  const api=new ExactCode();
  api.snapshots.set('fixture',{sources:new Map([
    ['a.py',Buffer.from('a'.repeat(30000))],
    ['b.py',Buffer.from('b'.repeat(30000))],
  ]),entities:[],manifest:[]});
  const result=api.query('fixture',[{file:'a.py'},{file:'b.py'},{file:'a.py'}]);
  assert.deepEqual(result.results.map(x=>x.status),['unique','deferred','unique']);
  assert.equal(result.files.length,1);
  assert.equal(result.files[0].content.length,30000);
  assert.equal(result.results[1].reason,'FILE_READ_BUDGET');
});
test('exact snapshot contract against real SEM parser',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-exact-test-'));
  try {
    const source='// café\nexport function same() { return "é"; }\n';
    await fs.writeFile(path.join(root,'a.ts'),source);
    await fs.writeFile(path.join(root,'b.ts'),source);
    const api=new ExactCode({semBin});
    const s=await api.capture(root,['b.ts','a.ts']);
    assert.deepEqual(await api.capture(root,['a.ts','b.ts','a.ts']),s);
    const r=api.resolve(s.revision,'same');
    assert.equal(r.status,'ambiguous'); assert.equal(r.matches.length,2);
    assert.equal(api.resolve(s.revision,'Same').status,'not_found');
    const e=r.matches[0],read=api.read(s.revision,e.id);
    const selectors=[{name:'same'},{name:'same',file:'a.ts'},{id:e.id},{name:'missing'}];
    const batch=api.query(s.revision,selectors);
    assert.deepEqual(api.query(s.revision,selectors),batch);
    assert.deepEqual(batch.results.map(x=>x.status),['ambiguous','unique','unique','not_found']);
    assert.equal(batch.sources.length,1);
    const mixed=api.query(s.revision,[{file:'a.ts'},{name:'same',file:'b.ts'},{file:'a.ts'}]);
    assert.equal(mixed.files.length,1);
    assert.equal(mixed.files[0].content,source);
    assert.equal(mixed.sources.length,1);
    assert.equal(api.query(s.revision,[{file:'absent.ts'}]).results[0].status,'not_found');
    assert.deepEqual({revision:s.revision,...batch.sources[0]},read);
    assert.throws(()=>api.query(s.revision,[]),/INVALID_SELECTORS/);
    assert.throws(()=>api.query(s.revision,[{name:'same',id:e.id}]),/INVALID_SELECTOR/);
    assert.throws(()=>api.query(s.revision,[{name:'same',file:42}]),/INVALID_SELECTOR/);
    assert.equal(read.content,Buffer.from(source).subarray(e.start,e.end).toString());
    const replacement='export function same() { return 42; }';
    const prepared=api.prepare(s.revision,[{id:e.id,content:replacement}]);
    assert.equal(prepared.status,'prepared_not_applied');
    assert.equal(prepared.files[0].content,source.replace(read.content,replacement));
    assert.equal(await fs.readFile(path.join(root,'a.ts'),'utf8'),source);
    assert.throws(()=>api.prepare(s.revision,[{id:e.id,content:''},{id:e.id,content:''}]),/OVERLAPPING/);
    await fs.writeFile(path.join(root,'a.ts'),'export function changed() {}');
    assert.deepEqual(api.read(s.revision,e.id),read);
    assert.deepEqual(api.query(s.revision,selectors),batch);
    const changed=await api.capture(root,['a.ts','b.ts']);
    assert.notEqual(changed.revision,s.revision);
    assert.throws(()=>api.read(changed.revision,e.id),/UNKNOWN_ENTITY/);
    assert.throws(()=>api.read('missing',e.id),/UNKNOWN_SNAPSHOT/);
    await assert.rejects(api.capture(root,['../outside.ts']),/INVALID_PATH/);
    await fs.symlink(path.join(root,'a.ts'),path.join(root,'link.ts'));
    await assert.rejects(api.capture(root,['link.ts']),/SYMLINK/);
    await assert.rejects(new ExactCode({semBin,maxBytes:1}).capture(root,['a.ts']),/SCOPE_TOO_LARGE/);
    const limited=new ExactCode({semBin,maxSnapshots:1});
    await limited.capture(root,['a.ts']);
    await assert.rejects(limited.capture(root,['b.ts']),/CAPACITY/);
  } finally {await fs.rm(root,{recursive:true,force:true});}
});
