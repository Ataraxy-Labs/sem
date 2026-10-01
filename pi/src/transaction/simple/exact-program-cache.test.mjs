import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import {execFileSync} from 'node:child_process';
import {ExactCode} from './exact-code.mjs';
import {rememberExactSource} from './exact-program-cache.mjs';
import {McpClient} from '../../bridge/mcp-client.ts';
import config from '../../../config/simple-transaction.mjs';
const semBin=process.env.SEM_TEST_BIN||'sem';

test('MCP explicit program inputs edit two files without model-facing reads',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-scoped-integration-'));
  const server=config.servers[0];
  const client=new McpClient({...server,cwd:root,env:{...server.env,SEM_JEV_DISABLED:'1',
    PATH:path.dirname(semBin)+path.delimiter+process.env.PATH}});
  try {
    execFileSync('git',['init','-q',root]);
    for(const file of ['a.py','b.py']) await fs.writeFile(path.join(root,file),'def alpha():\n    return 1\n');
    await client.start();
    const result=await client.callTool('weave_program',{files:['a.py','b.py'],code:`
      if(files.length!==2) throw Error('expected two inputs');
      const selected=entities.filter(e=>e.entity.name==='alpha');
      if(selected.length!==2) throw Error('expected two functions');
      return {edits:selected.map(e=>({file:e.file,entity:e.entity,old:'return 1',new:'return 2'}))};
    `});
    assert.notEqual(result.isError,true);
    for(const file of ['a.py','b.py']) assert.match(await fs.readFile(path.join(root,file),'utf8'),/return 2/);
    assert.match(JSON.stringify(result),/program_inputs/);
  } finally {await client.stop();await fs.rm(root,{recursive:true,force:true});}
});

test('cache remembers only delivered complete source, not candidate or unread files',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-exact-program-'));
  try {
    await fs.writeFile(path.join(root,'a.py'),'def alpha():\n    return "α"\n');
    await fs.writeFile(path.join(root,'b.py'),'def beta():\n    return 2\n');
    const exact=new ExactCode({semBin}),snap=await exact.capture(root,['a.py','b.py']);
    const hashes=new Map(),entities=new Map(),files=new Map();
    rememberExactSource(exact,exact.query(snap.revision,[{contains:'return'}]),hashes,entities,files);
    assert.equal(entities.size,0);assert.equal(files.size,0);assert.equal(hashes.size,0);
    rememberExactSource(exact,exact.query(snap.revision,[{name:'alpha'}]),hashes,entities,files);
    assert.equal(entities.size,1);assert.equal(files.size,0);assert.deepEqual([...hashes.keys()],['a.py']);
    const body=[...entities.values()][0];assert.match(body.content,/α/);assert.equal(body.entity.start_line,1);
    rememberExactSource(exact,exact.query(snap.revision,[{file:'b.py'}]),hashes,entities,files);
    assert.equal(files.size,1);assert.match(files.get('b.py').content,/beta/);
    await fs.writeFile(path.join(root,'a.py'),'def changed():\n    return 3\n');
    const next=await exact.capture(root,['a.py']);
    rememberExactSource(exact,exact.query(next.revision,[{name:'changed'}]),hashes,entities,files);
    assert.deepEqual([...entities.values()].map(e=>e.entity.name),['changed']);
  } finally {await fs.rm(root,{recursive:true,force:true});}
});

test('MCP exact read feeds guarded program edits without a duplicate sem_plan read',async()=>{
  const root=await fs.mkdtemp(path.join(os.tmpdir(),'sem-program-integration-'));
  const server=config.servers[0];
  const client=new McpClient({...server,cwd:root,env:{...server.env,SEM_JEV_DISABLED:'1',SEM_CONCURRENT_VALIDATION:'1',
    PATH:path.dirname(semBin)+path.delimiter+process.env.PATH}});
  try {
    execFileSync('git',['init','-q',root]);
    const file=path.join(root,'a.py');await fs.writeFile(file,'def alpha():\n    return 1\n');
    await client.start();
    await client.callTool('sem_exact',{op:'query',files:['a.py'],selectors:[{name:'alpha'}]});
    const result=await client.callTool('weave_program',{code:'return {edits:entities.map(d=>({file:d.file,old:d.content,new:d.content.replace("return 1","return 2")}))};'});
    assert.notEqual(result.isError,true);assert.match(await fs.readFile(file,'utf8'),/return 2/);
    await client.callTool('sem_exact',{op:'query',files:['a.py'],selectors:[{name:'alpha'}]});
    await fs.appendFile(file,'# external change\n');
    await assert.rejects(client.callTool('weave_program',{code:'return {edits:entities.map(d=>({file:d.file,old:d.content,new:d.content.replace("return 2","return 3")}))};'}),/Source changed/);
    assert.match(await fs.readFile(file,'utf8'),/return 2/);
  } finally {await client.stop();await fs.rm(root,{recursive:true,force:true});}
});
