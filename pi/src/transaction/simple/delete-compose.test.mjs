import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {normalizeEdits} from './normalize-edits-simple.mjs';

test('nested deletion composes with disjoint anchors in either order and rejects overlap',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-delete-compose-'));
 const source='namespace n {\nvoid keep() { old(); }\nvoid remove() { gone(); }\n}\n';
 const api={outline:async()=>({entities:[
  {name:'n',type:'module',start_line:1,end_line:4},
  {name:'keep',type:'function',parent_name:'n',start_line:2,end_line:2},
  {name:'remove',type:'function',parent_name:'n',start_line:3,end_line:3}]})};
 const deletion={file:'a.cpp',entity:{name:'remove',entity_type:'function'},op:'delete'};
 const change={file:'a.cpp',old:'old()',new:'fresh()'};
 try {
  await fs.writeFile(path.join(cwd,'a.cpp'),source);
  for(const batch of [[change,deletion],[deletion,change]]) {
   const result=await normalizeEdits(batch,cwd,api);
   assert.equal(result.edits.length,1);
   assert.equal(result.edits[0].entity.name,'n');
   assert.equal(result.edits[0].content,'namespace n {\nvoid keep() { fresh(); }\n\n}');
   assert.equal(await fs.readFile(path.join(cwd,'a.cpp'),'utf8'),source);
  }
  for(const batch of [[deletion,{...change,old:'gone()'}],[{...change,old:'gone()'},deletion]])
   await assert.rejects(normalizeEdits(batch,cwd,api),/CONFLICTING_TEXT_EDITS/);
  const direct=await normalizeEdits([deletion],cwd,api);
  assert.equal(direct.edits[0].op,'delete');
 } finally {await fs.rm(cwd,{recursive:true});}
});
