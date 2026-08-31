#!/usr/bin/env node
// Tiny fake MCP server for unit-testing the JSON-RPC framing in mcp-client.ts.
// Speaks newline-delimited JSON-RPC 2.0. Behavior is driven by env vars so
// tests can exercise timeout / crash / isError paths without extra args.

import { existsSync, writeFileSync } from "node:fs";

const SLOW_METHOD = process.env.FAKE_MCP_SLOW_METHOD; // method name to never answer
const HANG_TOOLS_LIST = process.env.FAKE_MCP_HANG_TOOLS_LIST === "1"; // tools/list itself never answers
const CRASH_ALWAYS = process.env.FAKE_MCP_CRASH_ALWAYS === "1";
// Crashes only on the first run (no marker file yet); a respawned process
// sees the marker and behaves normally. Lets a test observe recovery.
const CRASH_ONCE_MARKER = process.env.FAKE_MCP_CRASH_ONCE_MARKER;
const shouldCrashThisRun = CRASH_ALWAYS || (CRASH_ONCE_MARKER !== undefined && !existsSync(CRASH_ONCE_MARKER));

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
        serverInfo: { name: "fake-mcp", version: "0.0.0" },
      },
    });
    if (shouldCrashThisRun) {
      if (CRASH_ONCE_MARKER) writeFileSync(CRASH_ONCE_MARKER, "crashed-once");
      setTimeout(() => process.exit(1), 20);
    }
    return;
  }

  if (msg.method === "tools/list") {
    if (HANG_TOOLS_LIST) {
      return; // Never respond; exercises registerServerTools's listTools() timeout path.
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        tools: [
          {
            name: "echo",
            description: "Echo the given text back",
            inputSchema: {
              type: "object",
              properties: { text: { type: "string", description: "Text to echo" } },
              required: ["text"],
            },
          },
        ],
      },
    });
    return;
  }

  if (msg.method === "tools/call") {
    if (msg.params?.name === SLOW_METHOD) {
      return; // Never respond; exercises the client-side timeout.
    }
    if (msg.params?.name === "fail") {
      send({
        jsonrpc: "2.0",
        id: msg.id,
        result: { content: [{ type: "text", text: "boom" }], isError: true },
      });
      return;
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        content: [{ type: "text", text: `echo: ${msg.params?.arguments?.text ?? ""}` }],
      },
    });
    return;
  }

  send({
    jsonrpc: "2.0",
    id: msg.id,
    error: { code: -32601, message: `Method not found: ${msg.method}` },
  });
}
