import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {normalizeEdits} from './normalize-edits-simple.mjs';
test('range old/new is exact and malformed range edits cannot generate undefined source',async t=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'range-contract-'));
 t.after(()=>fs.rm(cwd,{recursive:true,force:true}));
 const source='group {\n\tLineColor black\n}\n';
 await fs.writeFile(path.join(cwd,'a.skin'),source);
 const api={outline:async()=>({entities:[]})};
 const base={file:'a.skin',range:{start_line:2,end_line:2}};
 const valid={...base,old:'\tLineColor black',new:'\tLineColor red'};
 const planned=await normalizeEdits([valid],cwd,api);
 assert.deepEqual(planned.textEdits,[{file:'a.skin',old:valid.old,new:valid.new}]);
 for(const edit of [base,{...base,old:valid.old},{...base,new:'x'},
   {...valid,content:'different'},{...valid,old:'LineColor black'},
   {...base,range:{start_line:0,end_line:2},content:'x'},
   {...base,range:{start_line:2,end_line:99},content:'x'},
   {...base,range:{start_line:3,end_line:2},content:'x'}]) {
  await assert.rejects(normalizeEdits([edit],cwd,api),/RANGE/);
  assert.equal(await fs.readFile(path.join(cwd,'a.skin'),'utf8'),source);
 }
 const deletion=await normalizeEdits([{...valid,new:''}],cwd,api);
 assert.equal(deletion.textEdits[0].new,'');
});
