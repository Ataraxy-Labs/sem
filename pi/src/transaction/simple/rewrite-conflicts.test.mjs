import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {normalizeEdits} from './normalize-edits-simple.mjs';
test('duplicate orphan import operations reject before application with operation indices',async t=>{
 const root=await fs.mkdtemp(path.join(os.tmpdir(),'rewrite-conflict-'));
 t.after(()=>fs.rm(root,{recursive:true,force:true}));
 const source='import p.Old.Type;\nclass A {}\n';
 await fs.writeFile(path.join(root,'A.java'),source);
 const api={outline:async()=>({entities:[]})};
 const edits=[{file:'A.java',old:'import p.Old.Type;\n',new:''},
  {file:'A.java',old:'import p.Old.Type;\n',new:'import p.Type;\n'}];
 for(const batch of [edits,edits.toReversed()]) {
  await assert.rejects(normalizeEdits(batch,root,api),/edits\[0\] and edits\[1\].*no writes applied/);
  assert.equal(await fs.readFile(path.join(root,'A.java'),'utf8'),source);
 }
});
