import {test} from 'node:test';
import assert from 'node:assert/strict';
import {ExactCode} from './exact-code.mjs';
import {indexEntities} from './entity-lookup.mjs';

function fixture() {
  const api=new ExactCode();
  const text='// é target\nclass A { f() { target(); } g() { target(); } }';
  const bytes=Buffer.from(text);
  const f=bytes.indexOf('f()'), g=bytes.indexOf('g()');
  const entities=[
    {id:'a',name:'A',type:'class',file:'a.ts',start:bytes.indexOf('class'),end:bytes.length},
    {id:'f',name:'f',type:'method',file:'a.ts',start:f,end:g-1},
    {id:'g',name:'g',type:'method',file:'a.ts',start:g,end:bytes.length-2},
  ];
  api.snapshots.set('r',{sources:new Map([['a.ts',bytes]]),entities,...indexEntities(entities)});
  return {api,bytes};
}
test('literal discovery returns smallest editable bodies, preserves uncovered UTF8 byte ranges',()=>{
  const {api,bytes}=fixture();
  const q=api.query('r',[{contains:'target',view:'source'}]);
  assert.equal(q.results[0].status,'partial');
  assert.equal(q.results[0].match_count,3);
  assert.deepEqual(q.results[0].source_ids,['f','g']);
  const range=q.results[0].uncovered[0];
  assert.equal(bytes.subarray(range.start_byte,range.end_byte).toString(),'target');
  assert.equal(q.sources.length,2);
  assert(q.sources.every(s=>s.complete&&!s.content.includes('class A')));
  const prepared=api.prepare('r',[{id:'f',content:'f() { fixed(); }'}]);
  assert.match(prepared.files[0].content,/fixed/);
  assert.match(prepared.files[0].content,/g\(\) \{ target/);
});
test('batch deduplicates source, unknown paths do not match, invalid selectors reject',()=>{
  const {api}=fixture();
  const q=api.query('r',[{contains:'target',file:'a.ts',view:'source'},{contains:'target',file:'a.ts',view:'source'},{contains:'target',file:'b.ts'}]);
  assert.equal(q.sources.length,2);
  assert.equal(q.results[2].status,'not_found');
  for(const selector of [{contains:''},{contains:3},{contains:'x',name:'f'},{contains:'x',file:3},{contains:'x',view:'auto'},{contains:'x',offset:-1}])
    assert.equal(api.query('r',[selector]).results[0].error.code,'INVALID_SELECTOR');
});
test('oversized bodies are deferred, never silently truncated',()=>{
  const {api}=fixture();
  const bytes=Buffer.from('target'+'x'.repeat(50000));
  const e={id:'large',name:'large',file:'large.ts',type:'function',start:0,end:bytes.length};
  api.snapshots.set('r',{sources:new Map([['large.ts',bytes]]),entities:[e],...indexEntities([e])});
  const q=api.query('r',[{contains:'target',view:'source'}]);
  assert.equal(q.results[0].status,'partial');
  assert.equal(q.results[0].deferred[0].id,'large');
  assert.equal(q.sources.length,0);
  assert.equal(api.read('r','large').content.length,bytes.length);
});
test('broad discovery returns no bodies; IDs hydrate in one batch from the same snapshot',()=>{
  const {api}=fixture();
  const result=api.query('r',[{contains:'target'}]);
  assert.equal(result.sources.length,0);
  assert.equal(result.results[0].uncovered_count,1);
  const candidates=result.results[0].candidates;
  assert.deepEqual(candidates.map(e=>e.id),['f','g']);
  assert(candidates.every(e=>e.bytes>0&&e.preview.includes('target')));
  const selected=api.query('r',candidates.map(e=>({id:e.id})));
  assert.equal(selected.sources.length,2);
  assert(selected.sources.every(e=>e.complete));
});
test('candidate paging includes uncovered matches and never implies complete coverage',()=>{
  const api=new ExactCode();
  const text='target\n'.repeat(105);
  api.snapshots.set('r',{sources:new Map([['a.txt',Buffer.from(text)]]),entities:[],...indexEntities([])});
  const first=api.query('r',[{contains:'target'}]).results[0];
  assert.equal(first.uncovered_count,105);
  assert.equal(first.uncovered.length,100);
  assert.equal(first.next_offset,100);
  const last=api.query('r',[{contains:'target',offset:first.next_offset}]).results[0];
  assert.equal(last.uncovered.length,5);
  assert.equal(last.next_offset,null);
  assert.equal(last.status,'partial');
});
test('uncovered matches can be read without reopening whole files',()=>{
  const {api,bytes}=fixture();
  const discovery=api.query('r',[{contains:'target'}]);
  const {file,start_byte,end_byte}=discovery.results[0].uncovered[0];
  const q=api.query('r',[{file,start_byte,end_byte},{file,start_byte,end_byte}]);
  assert.equal(q.ranges.length,1);
  assert.equal(q.ranges[0].content,'target');
  assert.equal(q.ranges[0].scope,'requested_range_only');
  assert.equal(q.files,undefined);
  const accented=bytes.indexOf(Buffer.from('é'));
  const invalidUtf8=api.query('r',[{file,start_byte:accented+1,end_byte:accented+2}]);
  assert.equal(invalidUtf8.results[0].error.code,'INVALID_UTF8_RANGE');
  assert.equal(invalidUtf8.results[0].complete,false);
  assert.equal(invalidUtf8.ranges,undefined);
  const partial=api.query('r',[{file,start_byte:0,end_byte:bytes.length+1},{file}]);
  assert.deepEqual(partial.results.map(x=>x.status),['error','unique']);
  assert.equal(partial.results[0].error.code,'INVALID_SOURCE_RANGE');
  assert.equal(partial.results[0].error.byte_length,bytes.length);
  assert.equal(partial.results[0].complete,false);
  assert.equal(partial.ranges,undefined);
  assert.equal(partial.files[0].content,bytes.toString('utf8'));
});
