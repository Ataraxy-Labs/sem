import test from 'node:test';
import assert from 'node:assert/strict';
import {compactResponse, toolResult} from './compact-response.mjs';

test('deduplicates exact source within this response, retaining identity',()=>{
  const body='function f() { /* '+'source '.repeat(100)+' */ }';
  const value={files:[{file:'x.ts',content:'// 😀\n'+body+'\n'}],
    definitions:[{file:'x.ts',entity:{name:'f'},content:body}]};
  const result=compactResponse(value), d=result.definitions[0], ref=d.content_source;
  assert.equal(result.files[0].content.slice(ref.start_utf16,ref.end_utf16),value.definitions[0].content);
  assert.deepEqual(d.entity,{name:'f'});
  assert.equal(value.definitions[0].content,body);
});
test('compaction never increases serialized response size, including metadata',()=>{
  for(const length of [0,1,20,100,200,400,1000]) {
    const content='x'.repeat(length);
    const value={files:[{file:'x',content}],definitions:[{file:'x',content}]};
    assert.ok(Buffer.byteLength(JSON.stringify(compactResponse(value)))<=Buffer.byteLength(JSON.stringify(value)));
    if(length<100) assert.deepEqual(compactResponse(value),value);
  }
});
test('never deduplicates against missing, truncated or mismatching files',()=>{
  for(const files of [[],[{file:'x',content:'abc',truncated:true}],[{file:'x',content:'def'}]]) {
    const value={files,definitions:[{file:'x',content:'abc'}]};
    assert.deepEqual(compactResponse(value),value);
  }
});
test('truncated definitions and explicit receipt responses stay intact',()=>{
  const value={files:[{file:'x',content:'abc'}],definitions:[{file:'x',content:'abc',truncated:true},{file:'x',unchanged:true,receipt:'r'}]};
  assert.deepEqual(compactResponse(value),value);
});
test('MCP sends one complete representation including errors',()=>{
  const value={error:'validation unavailable',pass:null};
  const result=toolResult(value);
  assert.equal(result.structuredContent,undefined);
  assert.deepEqual(JSON.parse(result.content[0].text),value);
});
