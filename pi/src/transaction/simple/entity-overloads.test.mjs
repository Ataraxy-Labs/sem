import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {generateEdits} from './edit-program.mjs';
import {selectEntity} from './repair-contract.mjs';
import {normalizeEdits} from './normalize-edits-simple.mjs';
test('overloaded entity remains addressable through generation and normalization',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'sem-overloads-'));
 const outline=[1,2].map(start_line=>({name:'create',type:'method',start_line,end_line:start_line}));
 const rows=outline.map(entity=>({file:'a.java',entity,content:'void create() { old(); }'}));
 try {
  await fs.writeFile(path.join(cwd,'a.java'),rows.map(r=>r.content).join('\n'));
  const generated=await generateEdits('return {edits:[replaceEntity(entities[1],"old","new")]};',rows);
  assert.equal(selectEntity(outline,generated.edits[0].entity),outline[1]);
  assert.throws(()=>selectEntity(outline,{...generated.edits[0].entity,start_line:3}),/Ambiguous/);
  const normalized=await normalizeEdits(generated.edits,cwd,{outline:async()=>({entities:outline})});
  assert.equal(normalized.edits[0].entity.ordinal,1);
  assert.match(normalized.edits[0].content,/new/);
 } finally {await fs.rm(cwd,{recursive:true,force:true});}
});
