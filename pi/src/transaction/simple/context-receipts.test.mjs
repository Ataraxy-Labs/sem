import test from 'node:test';
import assert from 'node:assert/strict';
import {ContextReceipts} from './context-receipts.mjs';
const definition={file:'f.ts',entity:{name:'f'},content:'function f() { /* '+ 'retained source '.repeat(30)+' */ }'};

test('never assumes that previous source is retained without explicit epoch',()=>{
  const cache=new ContextReceipts();
  for(let i=0;i<3;i++)assert.equal(cache.present([definition])[0].content,definition.content);
});
test('unchanged source is referenced only in acknowledged epoch',()=>{
  const cache=new ContextReceipts(),args={context_epoch:'session-1'};
  assert.equal(cache.present([definition],{...args,...cache.receipt()})[0].content,definition.content);
  const repeat=cache.present([definition],{...args,...cache.receipt()})[0];
  assert.equal(repeat.content,undefined);
  assert.equal(repeat.reused,true);
  assert.equal(repeat.context_epoch,'session-1');
});
test('changed body and changed locator are always delivered',()=>{
  const cache=new ContextReceipts(),args={context_epoch:'a'};
  cache.present([definition],args);
  for(const changed of [{...definition,content:'function f() {return 1}'},{...definition,file:'g.ts'}]) {
    assert.equal(cache.present([changed],{...args,...cache.receipt()})[0].content,changed.content);
  }
});
test('context loss and explicit refresh restore source',()=>{
  const cache=new ContextReceipts();
  for(const args of [{context_epoch:'a'},{context_epoch:'b'},{context_epoch:'b',refresh_source:true},{}]) {
    assert.equal(cache.present([definition],args)[0].content,definition.content);
  }
});
test('truncated source is never acknowledged; memory is bounded',()=>{
  const cache=new ContextReceipts(1),args={context_epoch:'a'};
  const truncated={...definition,truncated:true};
  cache.present([truncated],args);
  assert.equal(cache.present([truncated],args)[0].content,definition.content);
  cache.present([definition],args);
  cache.present([{...definition,file:'b'}],{...args,...cache.receipt()});
  assert.equal(cache.receipts.size,1);
  assert.equal(cache.present([definition],{...args,...cache.receipt()})[0].content,definition.content);
});
test('lost or unacknowledged response cannot authorize omission',()=>{
  const cache=new ContextReceipts(),args={context_epoch:'a'};
  cache.present([definition],args);
  assert.equal(cache.present([definition],args)[0].content,definition.content);
  assert.equal(cache.present([definition],{...args,ack_sequence:999})[0].content,definition.content);
  assert.equal(cache.present([definition],{...args,...cache.receipt()})[0].reused,true);
});
test('malformed epoch rejected',()=>{
  for(const context_epoch of ['',3,'x'.repeat(81)]) {
    assert.throws(()=>new ContextReceipts().present([definition],{context_epoch}));
  }
});
test('tiny bodies are cheaper to resend than reference',()=>{
  const cache=new ContextReceipts(),args={context_epoch:'a'};
  const tiny={...definition,content:'function f() {}'};
  cache.present([tiny],args);
  assert.equal(cache.present([tiny],{...args,...cache.receipt()})[0].content,tiny.content);
});
test('file bundles require acknowledgement and changed files are returned',()=>{
  const cache=new ContextReceipts(),args={context_epoch:'a'};
  const bundle={definitions:[definition],files:[{file:'file.ts',content:definition.content,truncated:false}]};
  const first=cache.presentBundle(bundle,args);
  const second=cache.presentBundle(bundle,{...args,...first.context_receipt});
  assert.equal(second.files[0].reused,true);
  assert.equal(second.definitions[0].reused,true);
  assert.equal('_source_kind' in first.files[0],false);
  const changed={...bundle,files:[{...bundle.files[0],content:'changed'}]};
  assert.equal(cache.presentBundle(changed,{...args,...second.context_receipt}).files[0].content,'changed');
});
