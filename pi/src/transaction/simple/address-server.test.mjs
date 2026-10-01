import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {McpClient} from '../../bridge/mcp-client.ts';
import config from '../../../config/simple-transaction.mjs';
test('edit program formats explicit Go inputs through MCP without an extra formatting turn',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'format-mcp-'));
 const server=config.servers[0];
 const client=new McpClient({...server,cwd,env:{...server.env,SEM_JEV_DISABLED:'1',
  PATH:path.dirname(process.env.SEM_TEST_BIN||'/usr/local/bin/sem')+path.delimiter+process.env.PATH}});
 try {
  execFileSync('git',['init','-q',cwd]);
  await fs.writeFile(path.join(cwd,'a.go'),'package example\n\nfunc Answer() int {\nreturn 1\n}\n');
  execFileSync('git',['add','a.go'],{cwd});
  execFileSync('git',['-c','user.name=Test','-c','user.email=test@example.test','-c','core.hooksPath=/dev/null','commit','-qm','base'],{cwd});
  await client.start();
  const result=await client.callTool('weave_program',{files:['a.go'],code:'const rows=entities.filter(r=>r.entity.name==="Answer"); if(rows.length!==1)throw Error("count"); return {edits:rows.map(r=>replaceEntity(r,"return 1","return 2")),format_files:["a.go"]};'});
  assert.notEqual(result.isError,true,JSON.stringify(result));
  const receipt=JSON.parse(result.content.filter(c=>c.type==='text').map(c=>c.text).join(''));
  assert.equal(receipt.formatting.ok,true,JSON.stringify(receipt));
  assert.equal(receipt.edit.succeeded,1);
  assert.equal(await fs.readFile(path.join(cwd,'a.go'),'utf8'),'package example\n\nfunc Answer() int {\n\treturn 2\n}\n');
  assert.notEqual(receipt.check.pass,true); // Formatting is not validation.
  const reviewResponse=await client.callTool('sem_exact',{op:'diff',files:['a.go']});
  const review=JSON.parse(reviewResponse.content.filter(c=>c.type==='text').map(c=>c.text).join(''));
  assert.match(review.diff,/-return 1/);assert.match(review.diff,/\+\treturn 2/);
  assert.equal(review.validation,'not_run');assert.equal(review.complete,true);
  await assert.rejects(client.callTool('weave_program',{files:['a.go'],code:'return {edits:[],format_files:["outside.go"]};'}),/FORMAT_OUTSIDE_EXPLICIT_SCOPE/);
 }finally{await client.stop();await fs.rm(cwd,{recursive:true});}
});

test('short-address query directly feeds a same-file edit batch without orientation',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'short-address-mcp-'));
 const server=config.servers[0];
 const client=new McpClient({...server,cwd,env:{...server.env,SEM_ENTITY_INTERFACE:'1',SEM_SHORT_ADDRESSES:'1',SEM_JEV_DISABLED:'1',
  PATH:path.dirname(process.env.SEM_TEST_BIN||'/usr/local/bin/sem')+path.delimiter+process.env.PATH}});
 const call=async(args)=>{
  const result=await client.callTool('sem_exact',args);assert.notEqual(result.isError,true,JSON.stringify(result));
  return JSON.parse(result.content.filter(x=>x.type==='text').map(x=>x.text).join(''));
 };
 try {
  execFileSync('git',['init','-q',cwd]);
  const source='def first():\n    return 1\n\ndef second():\n    return 2\n';
  await fs.writeFile(path.join(cwd,'a.py'),source);
  await client.start();
  const queried=await call({op:'query',files:['a.py'],selectors:[{name:'first'},{name:'second'}]});
  const addresses=queried.results.map(r=>r.address);
  assert.equal(addresses.length,2);
  assert.ok(addresses.every(a=>a.length<25));
  const changes=addresses.map((address,i)=>({address,old:`return ${i+1}`,new:`return ${(i+1)*11}`}));
  const applied=await call({op:'apply',changes});
  assert.equal(applied.status,'applied',JSON.stringify(applied));
  assert.equal(await fs.readFile(path.join(cwd,'a.py'),'utf8'),source.replace('return 1','return 11').replace('return 2','return 22'));
  await assert.rejects(client.callTool('sem_exact',{op:'apply',changes}),/STALE_SNAPSHOT/);
  const refreshed=await call({op:'query',files:['a.py'],selectors:[{name:'first'}]});
  assert.notEqual(refreshed.results[0].address,addresses[0]);
 }finally{await client.stop();await fs.rm(cwd,{recursive:true});}
});

test('address profile performs metadata-first read and guarded edit over MCP',async()=>{
 const cwd=await fs.mkdtemp(path.join(os.tmpdir(),'address-mcp-'));
 const server=config.servers[0];
 const client=new McpClient({...server,cwd,env:{...server.env,SEM_ENTITY_INTERFACE:'1',SEM_JEV_DISABLED:'1',
  PATH:path.dirname(process.env.SEM_TEST_BIN||'/usr/local/bin/sem')+path.delimiter+process.env.PATH}});
 const call=async(name,args)=>{
  const result=await client.callTool(name,args);assert.notEqual(result.isError,true);
  return JSON.parse(result.content.filter(x=>x.type==='text').map(x=>x.text).join(''));
 };
 try {
  execFileSync('git',['init','-q',cwd]);await fs.writeFile(path.join(cwd,'a.ts'),'export function value() {\n  return 1;\n}\n');
  await client.start();
  const listed=await client.listTools();assert.deepEqual(listed.map(t=>t.name).sort(),Object.keys(server.tools).sort());
  const cards=await call('sem_exact',{op:'orient',files:['a.ts'],select:{name:'value'}});
  assert.equal(cards.cards.length,1);assert.ok(!JSON.stringify(cards).includes('return 1'));
  const address=cards.cards[0].address;
  const queried=await call('sem_exact',{op:'query',files:['a.ts'],selectors:[{name:'value',file:'a.ts'}]});
  assert.match(queried.sources[0].content,/return 1/);
  assert.equal(queried.sources[0].address,address);
  assert.equal(queried.results[0].address,address);
  const source=await call('sem_exact',{op:'read',addresses:[address]});assert.match(source.sources[0].content,/return 1/);
  const applied=await call('sem_exact',{op:'apply',changes:[{address,old:'return 1',new:'return 2'}]});
  assert.equal(applied.status,'applied');assert.match(await fs.readFile(path.join(cwd,'a.ts'),'utf8'),/return 2/);
 } finally {await client.stop();await fs.rm(cwd,{recursive:true});}
});
