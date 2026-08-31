#!/usr/bin/env node
// Fake weave-mcp server for review-pass-4 finding #6: resyncCoordinationAfterRollback's
// `!claimResult.ok` fallback branch (weave-edit.ts) was implemented but never exercised
// by a test. Succeeds on the FIRST weave_claim_entity call for a given entity_name (the
// original, pre-rollback edit's claim), then fails every SUBSEQUENT claim call for that
// same entity_name -- simulating another agent grabbing it in the window between the
// original release and the rollback resync's re-claim attempt. The failure is a normal
// {isError:true} tool result (not a JSON-RPC-level throw), matching how a real "already
// claimed" business-logic refusal would come back, as distinct from the "-32602 not
// found" protocol-level throw the other fake fixtures in this directory reproduce.

import { appendFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG;

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

const claimCounts = new Map();

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
      result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fake-weave-mcp-reclaim-fails", version: "0.0.0" } },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    const args = msg.params?.arguments ?? {};
    logCall(toolName);

    if (toolName === "weave_claim_entity") {
      const count = (claimCounts.get(args.entity_name) ?? 0) + 1;
      claimCounts.set(args.entity_name, count);
      if (count > 1) {
        send({
          jsonrpc: "2.0",
          id: msg.id,
          result: { content: [{ type: "text", text: `entity '${args.entity_name}' is already claimed by another agent` }], isError: true },
        });
        return;
      }
      send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: "claimed" }] } });
      return;
    }

    send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: `${toolName}: ok` }] } });
    return;
  }

  send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `Method not found: ${msg.method}` } });
}
