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
    for (let i = 0; i < 34; i++) {
      const result = await client.callTool('sem_plan', { files: ['a.ts'] });
      assert.notEqual(result.isError, true);
    }
    const result = await client.callTool('sem_exact', {
      op: 'query', files: ['a.ts'], selectors: [{ file: 'a.ts', name: 'hello' }],
    });
    assert.match(JSON.stringify(result), /return 1/);
  } finally {
    await client.stop();
    await fs.rm(cwd, { recursive: true, force: true });
  }
});
