import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {McpClient} from '../../bridge/mcp-client.ts';
import config from '../../../config/simple-transaction.mjs';

test('recorded Iceberg first edit succeeds with explicit line source and corrected helper',
 {skip:!process.env.ICEBERG_TRACE||!process.env.ICEBERG_BASE},async()=>{
 const events=(await fs.readFile(process.env.ICEBERG_TRACE,'utf8')).trim().split('\n').map(JSON.parse);
 const original=events.find(r=>r.event?.type==='item.started'&&r.event.item?.tool==='weave_program').event.item.arguments;
 const args={...original,code:original.code.replaceAll('nested.content','nested.line_content')};
 assert.notEqual(args.code,original.code);
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'iceberg-rewrite-replay-'));
 const client=new McpClient({...config.servers[0],cwd,env:{...config.servers[0].env,SEM_JEV_DISABLED:'1',
  PATH:path.dirname(process.env.SEM_TEST_BIN)+path.delimiter+process.env.PATH}});
 try {
  execFileSync('git',['init','-q',cwd]);
  for(const file of args.files) {
   if(file.endsWith('/PhysicalType.java')||file.endsWith('/TestPhysicalType.java'))continue;
   const source=execFileSync('git',['show','HEAD:'+file],{cwd:process.env.ICEBERG_BASE});
   await fs.mkdir(path.dirname(path.join(cwd,file)),{recursive:true});
   await fs.writeFile(path.join(cwd,file),source);
  }
  await client.start();
  const result=await client.callTool('weave_program',args);
  assert.notEqual(result.isError,true,JSON.stringify(result));
  for(const file of args.files) {
   const source=await fs.readFile(path.join(cwd,file),'utf8');
   assert.ok(!source.includes('Variants.PhysicalType'),file);
   if(file.endsWith('/Variants.java'))assert.ok(!source.includes('enum PhysicalType'));
   if(file.endsWith('/PhysicalType.java'))assert.match(source,/public enum PhysicalType/);
  }
 } finally {await client.stop();await fs.rm(cwd,{recursive:true});}
});
