import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { McpClient } from '../../bridge/mcp-client.ts';
import config from '../../../config/simple-transaction.mjs';

test('portable simple config exposes exact tools and permits continued discovery', async () => {
  const cwd = await fs.mkdtemp(path.join(os.tmpdir(), 'sem-simple-server-'));
  const server = config.servers[0];
  const client = new McpClient({ ...server, cwd, env: {
    ...server.env,
    SEM_JEV_ENABLED: '', SEM_JEV_DISABLED: '',
    SEM_JEV_PROXY_TOKEN: '', TYPESAFE_API_KEY: '', SEM_JEV_TASK: '',
    PATH: process.env.SEM_TEST_BIN
      ? path.dirname(process.env.SEM_TEST_BIN) + path.delimiter + process.env.PATH
      : process.env.PATH,
  }});
  try {
    execFileSync('git', ['init', '-q', cwd]);
    await fs.writeFile(path.join(cwd, 'a.ts'), 'export function hello() { return 1; }\n');
    await client.start();
    const tools = await client.listTools();
    assert.deepEqual(tools.map(t => t.name).sort(), Object.keys(server.tools).sort());
    assert.deepEqual(config.sessionPolicy.activeBuiltins, []);
    const zero = await client.callTool('sem_plan', {files:['a.ts'],regex_patterns:['return 1'],max_entities:0});
    const zeroValue=JSON.parse(zero.content.filter(x=>x.type==='text').map(x=>x.text).join(''));
    assert.deepEqual(zeroValue.definitions,[]);
    assert.match(zeroValue.files[0].content,/return 1/);
    assert.ok(zeroValue.matches.results[0].returned>0);
    const namesOnly=await client.callTool('sem_plan',{entity_names:['hello'],max_entities:0});
    const namesValue=JSON.parse(namesOnly.content.filter(x=>x.type==='text').map(x=>x.text).join(''));
    assert.deepEqual(namesValue.definitions,[]);
    assert.deepEqual(namesValue.deferred_names,['hello']);
    assert.deepEqual(namesValue.unresolved_names,[]);
    for (let i = 0; i < 34; i++) {
      const result = await client.callTool('sem_plan', { files: ['a.ts'] });
      assert.notEqual(result.isError, true);
    }
    const result = await client.callTool('sem_exact', {
      op: 'query', files: ['a.ts'], selectors: [{ file: 'a.ts', name: 'hello' }],
    });
    assert.match(JSON.stringify(result), /return 1/);
    const literal = await client.callTool('sem_exact', {
      op: 'query', files: ['a.ts'], selectors: [{ contains: 'return 1' }],
    });
    assert.notEqual(literal.isError, true);
    assert.match(JSON.stringify(literal), /source_ids/);
    const compound=await client.callTool('sem_plan',{
      regex_patterns:['return 1'],max_entities:0,
      reads:{files:['a.ts'],selectors:[{name:'hello'}]},
    });
    const bundle=JSON.parse(compound.content.filter(x=>x.type==='text').map(x=>x.text).join(''));
    assert.equal(bundle.search.status,'ok');
    assert.equal(bundle.reads.status,'ok');
    assert.match(JSON.stringify(bundle.reads.result),/return 1/);
    assert.deepEqual(bundle.search.result.definitions,[]);
    await fs.writeFile(path.join(cwd,'large.ts'),'export function large() {\n'+'  // source context\n'.repeat(120)+'  return 1;\n}\n');
    const call=async args=>{
      const response=await client.callTool('sem_exact',args);
      assert.notEqual(response.isError,true);
      return JSON.parse(response.content.filter(x=>x.type==='text').map(x=>x.text).join(''));
    };
    const full=await call({op:'query',files:['large.ts'],selectors:[{name:'large'}]});
    const reused=await call({op:'query',revision:full.revision,selectors:[{id:full.sources[0].entity.id}],known_receipts:[full.sources[0].receipt]});
    assert.equal(reused.sources[0].content,undefined);assert.equal(reused.sources[0].reused,true);
    const restored=await call({op:'read',revision:full.revision,id:full.sources[0].entity.id,known_receipts:[full.sources[0].receipt],refresh_source:true});
    assert.match(restored.content,/return 1/);
    execFileSync('git',['add','.'],{cwd});
    execFileSync('git',['-c','user.name=Test','-c','user.email=test@example.test','-c','core.hooksPath=/dev/null','commit','-qm','base'],{cwd});
    const review=await call({op:'diff',files:['large.ts']});
    await fs.appendFile(path.join(cwd,'large.ts'),'// later change\n');
    const delta=await call({op:'diff',files:['large.ts'],since_diff:review.diff_id});
    assert.match(delta.diff,/\+\/\/ later change/);
  } finally {
    await client.stop();
    await fs.rm(cwd, { recursive: true, force: true });
  }
});
