import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

const __dirname = dirname(fileURLToPath(import.meta.url));
const previousMode = process.env.PI_SEM_MODE;
process.env.PI_SEM_MODE = "transaction";
const { startServersAndRegisterTools } = await import("../../extensions/pi-sem.ts");
if (previousMode === undefined) delete process.env.PI_SEM_MODE;
else process.env.PI_SEM_MODE = previousMode;

test("transaction mode activates only allowlisted server tools", async () => {
  const registered: string[] = [];
  const active: string[][] = [];
  const pi = {
    registerTool: (tool: { name: string }) => registered.push(tool.name),
    registerCommand: () => {},
    on: () => {},
    setActiveTools: (tools: string[]) => active.push(tools),
    getActiveTools: () => active.at(-1) ?? [],
    getAllTools: () => registered.map((name) => ({ name })),
  } as unknown as ExtensionAPI;
  const ctx = {
    ui: { notify: () => {} },
    cwd: process.cwd(),
  } as unknown as ExtensionContext;
  const fakeServer = join(__dirname, "fixtures", "fake-mcp-server.mjs");
  const result = await startServersAndRegisterTools(pi, ctx, {
    servers: [{ id: "transaction", command: process.execPath, args: [fakeServer], tools: { echo: true } }],
    // Transaction mode must discard ambient builtins even when configured.
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  });
  try {
    assert.deepEqual(registered, ["echo"]);
    assert.deepEqual(active, [["echo"]]);
    for (const forbidden of ["bash", "write", "sem_code", "weave_edit", "sem_read", "sem_find", "sem_grep"])
      assert.ok(!registered.includes(forbidden), `${forbidden} must not be registered`);
  } finally {
    await Promise.all(result.clients.map((client) => client.stop()));
  }
});

test("transaction mode fails closed when its server cannot start", async () => {
  const active: string[][] = [];
  const pi = {
    registerTool: () => {}, registerCommand: () => {}, on: () => {},
    setActiveTools: (tools: string[]) => active.push(tools),
    getActiveTools: () => active.at(-1) ?? [], getAllTools: () => [],
  } as unknown as ExtensionAPI;
  const ctx = { ui: { notify: () => {} }, cwd: process.cwd() } as unknown as ExtensionContext;
  const result = await startServersAndRegisterTools(pi, ctx, {
    servers: [{ id: "transaction", command: "/no/such/transaction-server", tools: { sem_plan: true, weave_transaction: true } }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  });
  assert.deepEqual(active, [[]]);
  assert.deepEqual(result.activeTools, []);
});
