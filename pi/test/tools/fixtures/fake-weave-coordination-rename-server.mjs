#!/usr/bin/env node
// Fake weave-mcp coordination server that reproduces the exact dogfood bug
// (dogfood round 1, finding 3): a successful rename left an orphaned claim because
// releasing by the claim-time (pre-rename) entity name failed with
// "entity '<old name>' not found". Simulates weave-mcp re-syncing its own
// tracked identity for a claim to whatever name is embedded in the content
// passed to weave_update_entity_content — so a subsequent weave_release_entity
// call using the STALE pre-rename name fails, while one using the new name
// succeeds. Logs every tools/call (method + entity_name argument) as JSON
// lines to FAKE_WEAVE_CALL_LOG so a test can assert the exact call sequence
// and identities used.

import { appendFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG;

function logCall(method, args) {
  if (CALL_LOG) appendFileSync(CALL_LOG, `${JSON.stringify({ method, entity_name: args?.entity_name ?? null })}\n`);
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

function extractFnName(content) {
  const m = /(?:fn|function)\s+([A-Za-z_][A-Za-z0-9_]*)/.exec(content ?? "");
  return m ? m[1] : null;
}

// weave-mcp's own tracked identity for the current claim — starts as
// whatever was claimed, shifts to the new name once update_entity_content
// re-syncs it (mirroring the real server's apparent behavior per the
// dogfood evidence).
let trackedName = null;

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
      result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fake-weave-mcp-rename", version: "0.0.0" } },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    const args = msg.params?.arguments ?? {};
    logCall(toolName, args);

    if (toolName === "weave_claim_entity") {
      trackedName = args.entity_name;
      send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: "claimed" }] } });
      return;
    }

    if (toolName === "weave_update_entity_content") {
      const newName = extractFnName(args.content);
      if (newName && newName !== trackedName) trackedName = newName;
      send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: "updated" }] } });
      return;
    }

    if (toolName === "weave_release_entity") {
      if (args.entity_name !== trackedName) {
        send({
          jsonrpc: "2.0",
          id: msg.id,
          error: { code: -32602, message: `entity '${args.entity_name}' not found in '${args.file_path}'` },
        });
        return;
      }
      send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: "released" }] } });
      return;
    }

    send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: `${toolName}: ok` }] } });
    return;
  }

  send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `Method not found: ${msg.method}` } });
}
