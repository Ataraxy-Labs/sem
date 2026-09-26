import {test} from 'node:test';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {compactSources} from './exact-code.mjs';
const h=s=>createHash('sha256').update(s).digest('hex');
test('same-response overlap preserves exact UTF-8 source and does not mutate input',()=>{
 const child='function hello() { /* '+ 'é'.repeat(200)+' */ }';
 const prefix='class Café {\n';const parent=prefix+child+'\n}';
 const make=(id,start,content,file='a.ts')=>({entity:{id,file,start,end:start+Buffer.byteLength(content)},content,sha256:h(content),complete:true});
 const sources=[make('child',Buffer.byteLength(prefix),child),make('parent',0,parent)];
 const before=JSON.stringify(sources);const result=compactSources(sources);
 assert.equal(JSON.stringify(sources),before);
 const ref=result[0].content_source;assert.equal(ref.source_id,'parent');
 assert.equal(Buffer.from(result[1].content).subarray(ref.start_byte,ref.end_byte).toString(),child);
 assert.equal(result[0].sha256,h(child));assert.equal(result[0].complete,true);
 assert.deepEqual(compactSources(sources),result);
 assert.equal(compactSources([make('c',Buffer.byteLength(prefix),child,'b.ts'),sources[1]])[0].content,child);
 assert.equal(compactSources([{...sources[0],sha256:'wrong'},sources[1]])[0].content,child);
});
