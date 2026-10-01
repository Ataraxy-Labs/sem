import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {ExactCode} from './exact-code.mjs';

test('malformed selector cannot discard valid siblings or fabricate missing source',async()=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'partial-query-'));
 try {
  await fs.writeFile(path.join(root,'a.py'),'def express():\n    return "environ"\n');
  const api=new ExactCode({semBin:process.env.SEM_TEST_BIN||'sem'});
  const snapshot=await api.capture(root,['a.py']);
  for(const malformed of [{start_byte:0,end_byte:7000,view:'source'},null,{name:'express',file:42}]) {
   for(const selectors of [[{contains:'express'},malformed,{name:'express'}],[malformed,{contains:'express'},{name:'express'}]]) {
    const result=api.query(snapshot.revision,selectors);
    assert.equal(result.results.filter(r=>r.status==='error').length,1);
    assert.equal(result.results.find(r=>r.status==='error').error.code,'INVALID_SELECTOR');
    assert.equal(result.results.find(r=>r.status==='error').complete,false);
    assert.equal(result.results.find(r=>r.status==='matched').total_candidates,1);
    assert.equal(result.sources.length,1);
    assert.match(result.sources[0].content,/return "environ"/);
    assert.equal(result.ranges,undefined);
   }
  }
 } finally {await fs.rm(root,{recursive:true,force:true});}
});
