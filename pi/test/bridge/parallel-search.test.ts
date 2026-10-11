import { test } from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI, ToolDefinition } from "@earendil-works/pi-coding-agent";
import { loadConfig } from "../../src/config/load.ts";
import { DEFAULT_CONFIG } from "../../src/config/default.ts";
import { registerServerTools } from "../../src/bridge/register.ts";

const configPath = fileURLToPath(new URL("../../config/parallel-search.mjs", import.meta.url));
const endpoint = "https://search.parallel.ai/mcp";
const userAgent = "pi-sem/0.1.0 (https://github.com/Ataraxy-Labs/sem)";

test("opt-in config preserves defaults and dispatches only allowlisted web tools over HTTP", async () => {
  const config = await loadConfig({ PI_SEM_CONFIG: configPath });
  assert.equal(await loadConfig({}), DEFAULT_CONFIG);
  assert.deepEqual(config.servers.slice(0, -1), DEFAULT_CONFIG.servers);
  assert.deepEqual(config.sessionPolicy, DEFAULT_CONFIG.sessionPolicy);
  assert.equal(config.systemPromptAddendum, DEFAULT_CONFIG.systemPromptAddendum);
  const parallel = config.servers.at(-1)!;
  assert.equal(parallel.command, process.execPath);
  assert.equal(parallel.args![1], endpoint);

  const requests: { method: string; url: string; userAgent?: string; authorization?: string }[] = [];
  const server = createServer(async (req, res) => {
    if (req.method !== "POST") { res.writeHead(405).end(); return; }
    let body = "";
    for await (const chunk of req) body += chunk;
    const message = JSON.parse(body);
    requests.push({ method: message.method, url: req.url!, userAgent: req.headers["user-agent"], authorization: req.headers.authorization });
    if (message.id === undefined) { res.writeHead(202).end(); return; }
    const result = message.method === "initialize"
      ? { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fixture", version: "1" } }
      : message.method === "tools/list"
        ? { tools: ["web_search", "web_fetch", "unlisted_tool"].map(name => ({ name, inputSchema: { type: "object" } })) }
        : { content: [{ type: "text", text: JSON.stringify(message.params) }] };
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ jsonrpc: "2.0", id: message.id, result }));
  });
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const authDir = await mkdtemp(join(tmpdir(), "pi-sem-parallel-auth-"));
  const tools: ToolDefinition[] = [];
  const api = { registerTool: (tool: ToolDefinition) => tools.push(tool) } as unknown as ExtensionAPI;
  const args = [...parallel.args!];
  args[1] = `http://127.0.0.1:${address.port}/mcp`;
  args.push("--allow-http");
  let client;
  try {
    const registered = await registerServerTools(api, {
      ...parallel, args, env: { MCP_REMOTE_CONFIG_DIR: authDir, NO_PROXY: "127.0.0.1" },
    });
    client = registered.client;
    assert.equal(registered.result.startError, undefined);
    assert.deepEqual(registered.result.registeredToolNames, ["parallel_web_search", "parallel_web_fetch"]);
    for (const [index, params] of [
      { objective: "Find Node documentation", search_queries: ["Node documentation"] },
      { urls: ["https://nodejs.org/api/fs.html"], objective: "Read file APIs" },
    ].entries()) {
      const result = await tools[index]!.execute(`call-${index}`, params, new AbortController().signal, undefined, {} as never);
      assert.deepEqual(JSON.parse((result.content[0] as { text: string }).text), {
        name: index === 0 ? "web_search" : "web_fetch", arguments: params,
      });
    }
    for (const method of ["initialize", "tools/list", "tools/call"]) {
      const observed = requests.filter(req => req.method === method);
      assert.ok(observed.length > 0, `observed ${method}`);
      for (const req of observed) {
        assert.equal(req.url, "/mcp");
        assert.equal(req.userAgent, userAgent);
        assert.equal(req.authorization, undefined);
      }
    }
    assert.equal(requests.filter(req => req.method === "tools/call").length, 2);
  } finally {
    await client?.stop();
    server.closeAllConnections();
    await new Promise<void>(resolve => server.close(() => resolve()));
    await rm(authDir, { recursive: true, force: true });
  }
});
