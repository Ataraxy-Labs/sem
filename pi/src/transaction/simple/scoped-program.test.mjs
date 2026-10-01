import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {ExactCode} from './exact-code.mjs';
import {prepareScopedProgram} from './scoped-program.mjs';

async function fixture(t) {
  const root=await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'sem-program-input-')));
  t.after(()=>fs.rm(root,{recursive:true,force:true}));
  await fs.writeFile(path.join(root,'a.ts'),'export function answer() { return 1; }\n');
  return {root,api:new ExactCode({semBin:process.env.SEM_TEST_BIN||'sem'})};
}
test('explicit inputs supply entities and files without a prior model read',async t=>{
  const {root,api}=await fixture(t);
  const result=await prepareScopedProgram(api,root,['a.ts'],`
    if(files.length!==1 || !entities.some(e=>e.entity.name==='answer')) throw Error('wrong inputs');
    return {edits:files.map(f=>({file:f.file,old:'return 1',new:'return 2'}))};
  `);
  assert.equal(result.generated.edits.length,1);
  assert.equal(result.receipt.inputs.length,1);
  assert.ok(!JSON.stringify(result.receipt).includes('export function'));
  assert.match(await fs.readFile(path.join(root,'a.ts'),'utf8'),/return 1/);
});
test('program scope rejects undeclared writes and permits declared missing creates',async t=>{
  const {root,api}=await fixture(t);
  await assert.rejects(prepareScopedProgram(api,root,['a.ts'],`return {edits:[{file:'b.ts',old:'x',new:'y'}]};`),/OUTSIDE_EXPLICIT_SCOPE/);
  const result=await prepareScopedProgram(api,root,['a.ts','new.ts'],`return {edits:[],creates:[{file:'new.ts',content:'export const x=1;'}]};`);
  assert.deepEqual(result.receipt.missing_files,['new.ts']);
  await assert.rejects(fs.stat(path.join(root,'new.ts')),/ENOENT/);
  await assert.rejects(prepareScopedProgram(api,root,['../escape.ts'],'return {edits:[]};'),/INVALID_PATH/);
  await fs.symlink(path.join(root,'a.ts'),path.join(root,'link.ts'));
  await assert.rejects(prepareScopedProgram(api,root,['link.ts'],'return {edits:[]};'),/SYMLINK/);
});
test('assertion failures and changed inputs fail before application',async t=>{
  const {root,api}=await fixture(t);
  await assert.rejects(prepareScopedProgram(api,root,['a.ts'],`throw Error('expected count mismatch');`),/expected count mismatch/);
  const original=api.capture.bind(api);
  api.capture=async (...args)=>{
    const result=await original(...args);
    await fs.writeFile(path.join(root,'a.ts'),'export const changed=1;');
    return result;
  };
  await assert.rejects(prepareScopedProgram(api,root,['a.ts'],'return {edits:[]};'),/PROGRAM_SOURCE_CHANGED/);
});
test('a missing destination appearing after capture is rejected',async t=>{
  const {root,api}=await fixture(t);
  const original=api.capture.bind(api);
  api.capture=async (...args)=>{
    const result=await original(...args);
    await fs.writeFile(path.join(root,'new.ts'),'occupied');
    return result;
  };
  await assert.rejects(prepareScopedProgram(api,root,['a.ts','new.ts'],'return {edits:[]};'),/PROGRAM_SOURCE_CHANGED/);
});
