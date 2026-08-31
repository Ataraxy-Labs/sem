#!/usr/bin/env node
// Fake weave-mcp coordination server for testing Coordinator's claim/release
// lifecycle (weave-edit-lock-leak.test.ts). Speaks the same
// newline-delimited JSON-RPC framing as the bridge's fake-mcp-server.mjs.
// Logs every tools/call method name it receives, one per line, to
// FAKE_WEAVE_CALL_LOG (env) or argv[2] (fallback for a Coordinator built
// before env passthrough existed) so a test can assert which coordination
// calls actually happened.

import { appendFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG ?? process.argv[2];

function logCall(method) {
  if (CALL_LOG) appendFileSync(CALL_LOG, `${method}\n`);
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let newlineIndex = buffer.indexOf("\n");
  while (newlineIndex !== -1) {
    const line = buffer.slice(0, newlineIndex);
    buffer = buffer.slice(newlineIndex + 1);
    handleLine(line);
    newlineIndex = buffer.indexOf("\n");
  }
});

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function handleLine(line) {
  const trimmed = line.trim();
  if (!trimmed) return;
  const msg = JSON.parse(trimmed);

  if (msg.method === "notifications/initialized") return;
  if (msg.method === "notifications/cancelled") return;

  if (msg.method === "initialize") {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "fake-weave-mcp", version: "0.0.0" },
      },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    logCall(toolName);
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: { content: [{ type: "text", text: `${toolName}: ok` }] },
    });
    return;
  }

  send({
    jsonrpc: "2.0",
    id: msg.id,
    error: { code: -32601, message: `Method not found: ${msg.method}` },
  });
}
