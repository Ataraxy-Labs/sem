#!/usr/bin/env node
// Fake weave-mcp coordination server for pi-sem's client-side follow-up to
// a weave-mcp fix: weave_claim_entity's response includes an `entity_id`,
// and weave_update_entity_content/weave_release_entity calls that carry a
// matching `entity_id` succeed regardless of what `entity_name` they were
// sent under -- simulating a fixed weave-mcp that addresses a claim by its
// own stable identity instead of re-resolving entity_name against the
// file's (possibly renamed) current content. Calls that omit entity_id, or
// send a mismatched one, fail exactly like an un-fixed server would after a
// rename -- a real "entity not found" JSON-RPC-level protocol error, not an
// {isError:true} tool result (matching every other fixture in this
// directory that simulates this failure mode).

import { appendFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG;
const STABLE_ENTITY_ID = "stable-entity-id-1";

function logCall(method, args) {
  if (CALL_LOG) {
    appendFileSync(
      CALL_LOG,
      `${JSON.stringify({ method, entity_name: args?.entity_name ?? null, entity_id: args?.entity_id ?? null })}\n`,
    );
  }
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
      result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fake-weave-mcp-entity-id", version: "0.0.0" } },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    const args = msg.params?.arguments ?? {};
    logCall(toolName, args);

    if (toolName === "weave_claim_entity") {
      const payload = { result: "claimed", dependency_warnings: [], entity_id: STABLE_ENTITY_ID };
      send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: JSON.stringify(payload) }] } });
      return;
    }

    if (toolName === "weave_update_entity_content" || toolName === "weave_release_entity") {
      if (args.entity_id === STABLE_ENTITY_ID) {
        send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: `${toolName}: ok by entity_id` }] } });
        return;
      }
      // No entity_id (or a mismatched one): simulate today's un-fixed
      // server, which resolves entity_name against the file's current
      // (by then renamed) content and fails to find the stale name.
      send({
        jsonrpc: "2.0",
        id: msg.id,
        error: { code: -32602, message: `entity '${args.entity_name}' not found in '${args.file_path}'` },
      });
      return;
    }

    send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: `${toolName}: ok` }] } });
    return;
  }

  send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `Method not found: ${msg.method}` } });
}
