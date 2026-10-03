import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {ParseCache} from './parse-cache.mjs';
import {ExactCode} from './exact-code.mjs';
test('parse cache is bounded, LRU, immutable and rejects invalid capacities',()=>{
  const c=new ParseCache({maxEntries:2,maxBytes:1000});
  const rows=[{name:'a'}];c.set('a',rows);rows[0].name='mutated';
  assert.equal(c.get('a')[0].name,'a');
  assert.throws(()=>{c.get('a')[0].name='bad';},TypeError);
  c.set('b',[]);c.get('a');c.set('c',[]);
  assert.equal(c.get('b'),undefined);assert.ok(c.get('a'));
  c.set('huge',[{name:'x'.repeat(2000)}]);assert.equal(c.get('huge'),undefined);
  assert.ok(c.bytes<=1000);
  assert.throws(()=>new ParseCache({maxEntries:0}));
});
test('overlapping scopes reuse parses; changed bytes and failures cannot reuse stale rows',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-parse-cache-test-'));
  const exact=new ExactCode();let calls=0;
  exact.parseEntityRows=async target=>{
    calls++;const data=await fs.readFile(target);
    if(data.toString()==='bad')return [{name:'x',type:'function',start_byte:0,end_byte:100}];
    return [{name:data.toString(),type:'function',start_byte:0,end_byte:data.length}];
  };
  try {
    await fs.writeFile(path.join(root,'a.py'),'alpha');await fs.writeFile(path.join(root,'b.py'),'beta');
    const a=await exact.capture(root,['a.py']);
    const both=await exact.capture(root,['a.py','b.py']);
    assert.equal(calls,2);assert.equal(exact.lastCaptureParsing.cache_hits,1);
    assert.notEqual(exact.resolve(a.revision,'alpha').matches[0].id,exact.resolve(both.revision,'alpha').matches[0].id);
    await fs.writeFile(path.join(root,'a.py'),'changed');
    const changed=await exact.capture(root,['a.py','b.py']);
    assert.equal(calls,3);assert.equal(exact.lastCaptureParsing.cache_hits,1);
    assert.equal(exact.resolve(changed.revision,'alpha').status,'not_found');
    assert.equal(exact.read(a.revision,exact.resolve(a.revision,'alpha').matches[0].id).content,'alpha');
    await fs.writeFile(path.join(root,'a.py'),'bad');
    await assert.rejects(exact.capture(root,['a.py']),/INVALID_ENTITY_RANGE/);
    await assert.rejects(exact.capture(root,['a.py']),/INVALID_ENTITY_RANGE/);
    assert.equal(calls,5);
  } finally {await fs.rm(root,{recursive:true,force:true});}
});
test('parallel cold parses preserve IDs and warm captures reparse only changed files',async t=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-parallel-parses-'));
 t.after(()=>fs.rm(root,{recursive:true,force:true}));
 const files=['a.py','b.py','c.py'];
 await Promise.all(files.map(f=>fs.writeFile(path.join(root,f),f)));
 const make=concurrency=>{
  const exact=new ExactCode({parseConcurrency:concurrency});
  exact.parseEntityRows=async target=>{
   await new Promise(r=>setTimeout(r,target.endsWith('a.py')?20:2));
   return [{name:'f',type:'function',start_byte:0,end_byte:(await fs.readFile(target)).length}];
  };
  return exact;
 };
 const sequential=make(1),parallel=make(3);
 const a=await sequential.capture(root,files),b=await parallel.capture(root,files);
 assert.equal(a.revision,b.revision);
 assert.deepEqual(sequential.get(a.revision).entities,parallel.get(b.revision).entities);
 await fs.writeFile(path.join(root,'b.py'),'changed');
 await parallel.capture(root,files);
 assert.equal(parallel.lastCaptureParsing.parser_calls,1);assert.equal(parallel.lastCaptureParsing.cache_hits,2);
 assert.throws(()=>new ExactCode({parseConcurrency:0}),/CONCURRENCY/);
});
