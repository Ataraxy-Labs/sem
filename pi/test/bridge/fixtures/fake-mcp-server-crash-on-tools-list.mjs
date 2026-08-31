#!/usr/bin/env node
// Fake MCP server that completes the initialize handshake normally, then
// exits without responding as soon as it receives tools/list. Used by
// register-tools-listtools-escape.review.test.ts to show that
// registerServerTools() does not capture a listTools() failure the same
// way it captures a start() failure.
//
// If FAKE_MCP_SPAWN_LOG is set, every process instance appends its own PID
// to that file on startup — this lets a test detect (and clean up) the
// zombie respawn that McpClient's onExit() fires unconditionally after
// *any* unexpected exit, even one the caller is about to abandon.

import { appendFileSync } from "node:fs";

const SPAWN_LOG = process.env.FAKE_MCP_SPAWN_LOG;
if (SPAWN_LOG) appendFileSync(SPAWN_LOG, `${process.pid}\n`);

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

  if (msg.method === "initialize") {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "fake-mcp-crash-on-list", version: "0.0.0" },
      },
    });
    return;
  }

  if (msg.method === "tools/list") {
    // Simulate a mid-session crash right as the client asks for the tool
    // catalog: exit without ever answering this request.
    process.exit(1);
  }
}
