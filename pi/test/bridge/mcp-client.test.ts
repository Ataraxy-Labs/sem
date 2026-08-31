import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { McpClient, McpClientCrashedError } from "../../src/bridge/mcp-client.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_SERVER = join(__dirname, "fixtures", "fake-mcp-server.mjs");

function makeClient(env: Record<string, string> = {}, requestTimeoutMs = 2000) {
  return new McpClient({
    id: "fake",
    command: process.execPath,
    args: [FAKE_SERVER],
    env,
    requestTimeoutMs,
  });
}

test("initialize handshake + tools/list returns the fake server's tool", async () => {
  const client = makeClient();
  await client.start();
  try {
    const tools = await client.listTools();
    assert.equal(tools.length, 1);
    const [tool] = tools;
    assert.ok(tool);
    assert.equal(tool.name, "echo");
    assert.equal(tool.inputSchema.type, "object");
  } finally {
    await client.stop();
  }
});

test("tools/call round-trips arguments through newline-JSON framing", async () => {
  const client = makeClient();
  await client.start();
  try {
    const result = await client.callTool("echo", { text: "hello" });
    assert.equal(result.isError, undefined);
    assert.equal(result.content[0]?.text, "echo: hello");
  } finally {
    await client.stop();
  }
});

test("isError result is passed through, not thrown", async () => {
  const client = makeClient();
  await client.start();
  try {
    const result = await client.callTool("fail", {});
    assert.equal(result.isError, true);
    assert.equal(result.content[0]?.text, "boom");
  } finally {
    await client.stop();
  }
});

test("a call the server never answers rejects on the per-call timeout", async () => {
  const client = makeClient({ FAKE_MCP_SLOW_METHOD: "stall" }, 200);
  await client.start();
  try {
    await assert.rejects(client.callTool("stall", {}), /timed out/);
  } finally {
    await client.stop();
  }
});

test("an aborted call rejects immediately via AbortSignal", async () => {
  const client = makeClient({ FAKE_MCP_SLOW_METHOD: "stall" }, 5000);
  await client.start();
  try {
    const controller = new AbortController();
    const pending = client.callTool("stall", {}, controller.signal);
    controller.abort();
    await assert.rejects(pending, /aborted/);
  } finally {
    await client.stop();
  }
});

test("a single crash after init triggers one restart and the client recovers", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-mcp-test-"));
  const marker = join(dir, "crashed-once");
  try {
    const client = makeClient({ FAKE_MCP_CRASH_ONCE_MARKER: marker }, 2000);
    await client.start();

    // The first spawn crashes ~20ms after init; wait for the client to
    // detect the exit and respawn, then confirm it's usable again.
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(client.isRunning, true, "client should have respawned after the single crash");

    const tools = await client.listTools();
    assert.equal(tools.length, 1);

    await client.stop();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("two consecutive crashes exhaust the one allowed restart and mark the client dead", async () => {
  const client = makeClient({ FAKE_MCP_CRASH_ALWAYS: "1" }, 2000);
  await client.start();

  // Give both the original process and its one respawn time to crash.
  await new Promise((resolve) => setTimeout(resolve, 400));
  assert.equal(client.isRunning, false, "client should be dead after its one restart also crashes");

  await assert.rejects(client.listTools(), McpClientCrashedError);
});
