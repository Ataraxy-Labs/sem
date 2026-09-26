import test from 'node:test';
import assert from 'node:assert/strict';
import {compactResponse, toolResult} from './compact-response.mjs';

test('deduplicates exact source within this response, retaining identity',()=>{
  const value={files:[{file:'x.ts',content:'// 😀\nfunction f() {}\n'}],
    definitions:[{file:'x.ts',entity:{name:'f'},content:'function f() {}'}]};
  const result=compactResponse(value), d=result.definitions[0], ref=d.content_source;
  assert.equal(result.files[0].content.slice(ref.start_utf16,ref.end_utf16),value.definitions[0].content);
  assert.deepEqual(d.entity,{name:'f'});
  assert.equal(value.definitions[0].content,'function f() {}');
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
