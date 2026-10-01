import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {formatFiles} from './format-files.mjs';
async function fixture(t){
  const root=await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(),'sem-format-')));
  t.after(()=>fs.rm(root,{recursive:true,force:true}));
  const source='package example\nfunc Answer() int {return 1}\n';
  for(const file of ['a.go','b.go'])await fs.writeFile(path.join(root,file),source);
  return {root,source};
}
test('gofmt changes only explicit files and is idempotent',async t=>{
  const {root,source}=await fixture(t);
  const result=await formatFiles(root,['a.go']);
  assert.deepEqual(result.changed,['a.go']);
  assert.equal(result.validation,'not_run');
  assert.equal(await fs.readFile(path.join(root,'b.go'),'utf8'),source);
  assert.deepEqual((await formatFiles(root,['a.go'])).changed,[]);
});
test('invalid scope and syntax fail without writing earlier files',async t=>{
  const {root,source}=await fixture(t);
  for(const file of ['../a.go','/a.go','a.py'])await assert.rejects(formatFiles(root,[file]),/EXPLICIT_GO/);
  await fs.symlink(path.join(root,'a.go'),path.join(root,'link.go'));
  await assert.rejects(formatFiles(root,['link.go']),/SYMLINK/);
  await fs.writeFile(path.join(root,'b.go'),'not go');
  await assert.rejects(formatFiles(root,['a.go','b.go']),/gofmt failed/);
  assert.equal(await fs.readFile(path.join(root,'a.go'),'utf8'),source);
});
test('later stale source rolls back only formatter-owned writes',async t=>{
  const {root,source}=await fixture(t);let calls=0;
  const result=await formatFiles(root,['a.go','b.go'],{format:async bytes=>{
    if(++calls===2)await fs.writeFile(path.join(root,'b.go'),'external writer');
    return Buffer.concat([bytes,Buffer.from('\n')]);
  }});
  assert.equal(result.ok,false);
  assert.deepEqual(result.restored,['a.go']);
  assert.deepEqual(result.unrestored,[]);
  assert.equal(await fs.readFile(path.join(root,'a.go'),'utf8'),source);
  assert.equal(await fs.readFile(path.join(root,'b.go'),'utf8'),'external writer');
});
