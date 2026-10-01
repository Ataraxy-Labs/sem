import {test} from 'node:test';
import assert from 'node:assert/strict';
import {ExactCode} from './exact-code.mjs';
import {indexEntities} from './entity-lookup.mjs';

function fixture(size=40) {
  const api=new ExactCode();
  const entities=['a','b'].map(id=>({id,name:'same',file:`${id}.ts`,type:'function',start:0,end:size}));
  api.snapshots.set('r',{entities,...indexEntities(entities),
    sources:new Map(entities.map(e=>[e.file,Buffer.from(e.id.repeat(size))])),manifest:[]});
  return api;
}
test('explicit ambiguous reads return every requested body without choosing a target',()=>{
  const api=fixture();
  assert.equal(api.query('r',[{name:'same'}]).sources.length,0);
  const selectors=[{name:'same',view:'source'},{name:'same',view:'source'}];
  const result=api.query('r',selectors);
  assert.deepEqual(result,api.query('r',selectors));
  assert.equal(result.sources.length,2);
  for(const r of result.results) {
    assert.equal(r.status,'ambiguous');
    assert.deepEqual(r.source_ids,['a','b']);
    assert.equal(r.complete,true);
    assert.equal(r.source_id,undefined);
  }
  assert.throws(()=>api.prepare('r',[{id:'same',content:'bad'}]),/INVALID_EDIT/);
});
test('ambiguous hydration shares the file budget and exposes deferred IDs',()=>{
  const api=fixture(30000);
  const result=api.query('r',[{name:'same',view:'source'},{file:'b.ts'}]);
  assert.deepEqual(result.results[0].source_ids,['a']);
  assert.deepEqual(result.results[0].deferred.map(e=>e.id),['b']);
  assert.equal(result.results[0].complete,false);
  assert.equal(result.results[1].status,'deferred');
  assert.equal(api.query('r',[{id:'b'}]).sources[0].content.length,30000);
  const afterFile=api.query('r',[{file:'a.ts'},{name:'same',view:'source'}]);
  assert.equal(afterFile.results[1].deferred.length,2);
});
test('candidate-only named reads never hydrate; invalid views fail explicitly',()=>{
  const api=fixture();
  const result=api.query('r',[{id:'a',view:'candidates'},{name:'missing',view:'source'},{name:'same',view:'all'}]);
  assert.equal(result.sources.length,0);
  assert.deepEqual(result.results.map(r=>r.status),['unique','not_found','error']);
});
