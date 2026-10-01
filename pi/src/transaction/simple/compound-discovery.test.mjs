import {test} from 'node:test';
import assert from 'node:assert/strict';
import {addCompoundDiscovery} from './compound-discovery.mjs';
const reads={files:['a.ts'],selectors:[{name:'hello'}]};
function setup(search,query) {
 const plan={schema:{properties:{}},description:'Search.',run:search};
 const exact={schema:{properties:{files:{type:'array'},selectors:{type:'array'}}},run:query};
 addCompoundDiscovery(plan,exact);return plan;
}
test('no reads preserves original behavior without any additional retrieval',async()=>{
 const p=setup(async p=>p,()=>{throw Error('unexpected query');});
 assert.deepEqual(await p.run({files:['a']},'/repo'),{files:['a']});
});
test('independent reads start before search finishes and receive only explicit scope',async()=>{
 let resolveSearch;
 const p=setup((args)=>{assert.equal(args.reads,undefined);return new Promise(r=>resolveSearch=r);},async args=>{
  assert.deepEqual(args,{op:'query',...reads});resolveSearch({matches:[]});return {complete:false};
 });
 const r=await p.run({regex_patterns:['foo'],reads},'/repo');
 assert.deepEqual(r.search,{status:'ok',result:{matches:[]}});
 assert.deepEqual(r.reads,{status:'ok',result:{complete:false}});
});
test('one failing branch cannot discard the other response',async()=>{
 const r=await setup(()=>{throw Error('search failed');},async()=>({sources:['source']})).run({reads},'/repo');
 assert.equal(r.search.status,'error');assert.equal(r.reads.status,'ok');
 const q=await setup(async()=>({matches:[]}),()=>{throw Error('invalid path');}).run({reads},'/repo');
 assert.equal(q.search.status,'ok');assert.equal(q.reads.error,'invalid path');
});
test('invalid read scopes fail before either branch executes',async()=>{
 const p=setup(()=>assert.fail(),()=>assert.fail());
 for(const value of [null,{}, {...reads,op:'apply'}, {...reads,files:[]}])
  await assert.rejects(p.run({reads:value},'/repo'),/INVALID_EXPLICIT_READ_BATCH/);
});
