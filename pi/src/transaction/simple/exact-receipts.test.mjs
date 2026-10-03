import test from 'node:test';
import assert from 'node:assert/strict';
import {presentExact} from './exact-receipts.mjs';
const source={entity:{id:'old-id',file:'a.py',name:'f',type:'function',start:0,end:2000},content:'x'.repeat(2000),complete:true};
test('exact source omitted only after explicit acknowledgement; refresh restores it',()=>{
  const first=presentExact({sources:[source]});
  assert.equal(first.sources[0].content,source.content);
  const options={known_receipts:[first.sources[0].receipt]};
  const second=presentExact({sources:[source]},options);
  assert.equal(second.sources[0].content,undefined);assert.equal(second.sources[0].reused,true);
  assert.equal(presentExact({sources:[source]},{...options,refresh_source:true}).sources[0].content,source.content);
  assert.ok(JSON.stringify(second).length<JSON.stringify(first).length/2);
});
test('unrelated revision changes reuse bodies but not IDs; changed bytes are resent',()=>{
  const receipt=presentExact(source).receipt;
  const changedID={...source,revision:'new',entity:{...source.entity,id:'new-id'}};
  const reused=presentExact(changedID,{known_receipts:[receipt]});
  assert.equal(reused.entity.id,'new-id');assert.equal(reused.content,undefined);
  assert.equal(presentExact({...changedID,content:'different'},{known_receipts:[receipt]}).content,'different');
});
test('same-response slices retain their full parent; partial and tiny source retained',()=>{
  const first=presentExact(source);
  const child={entity:{id:'child'},content_source:{kind:'same_response_utf8_slice',source_id:'old-id',start_byte:0,end_byte:10}};
  const result=presentExact({sources:[source,child]},{known_receipts:[first.receipt]});
  assert.equal(result.sources[0].content,source.content);
  const tiny=presentExact({...source,content:'x'});
  assert.equal(presentExact(tiny,{known_receipts:[tiny.receipt]}).content,'x');
  assert.equal(presentExact({...source,complete:false},{known_receipts:[first.receipt]}).content,source.content);
  assert.throws(()=>presentExact({}, {known_receipts:'bad'}),/INVALID_SOURCE_RECEIPTS/);
});
