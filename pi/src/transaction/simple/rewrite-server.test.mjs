import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {McpClient} from '../../bridge/mcp-client.ts';
import config from '../../../config/simple-transaction.mjs';

test('rewrite helper applies a repeated-line change through existing transaction guards',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'rewrite-mcp-'));
 const server=config.servers[0];
 const client=new McpClient({...server,cwd,env:{...server.env,SEM_JEV_DISABLED:'1',
  PATH:path.dirname(process.env.SEM_TEST_BIN||'/usr/local/bin/sem')+path.delimiter+process.env.PATH}});
 const source='def first():\n    return 1\n\ndef second():\n    return 1\n';
 try {
  execFileSync('git',['init','-q',cwd]);
  await fs.writeFile(path.join(cwd,'a.py'),source);
  await client.start();
  await assert.rejects(client.callTool('weave_program',{files:['a.py'],code:
   'return {edits:rewriteLines(files[0],"return 1",line=>line.replaceAll("return 1","return 2"),3)};'}),/expected 3/);
  assert.equal(await fs.readFile(path.join(cwd,'a.py'),'utf8'),source);
  const result=await client.callTool('weave_program',{files:['a.py'],code:
   'return {edits:rewriteLines(files[0],"return 1",line=>line.replaceAll("return 1","return 2"),2)};'});
  assert.notEqual(result.isError,true,JSON.stringify(result));
  assert.equal(await fs.readFile(path.join(cwd,'a.py'),'utf8'),source.replaceAll('return 1','return 2'));
  const receipt=JSON.parse(result.content.filter(c=>c.type==='text').map(c=>c.text).join(''));
  assert.notEqual(receipt.check.pass,true);
  assert.equal(receipt.program_inputs.source_visibility,'loaded_inside_program_not_sent_to_model');
 } finally {await client.stop();await fs.rm(cwd,{recursive:true});}
});
