#!/usr/bin/env node
// Fake weave-mcp server for review-pass-4 finding #6: resyncCoordinationAfterRollback's
// `!releaseResult.ok` fallback branch (weave-edit.ts) was implemented but never exercised
// by a test. Succeeds on the original edit's full claim/update/release cycle, and on the
// rollback resync's RE-claim, but fails every weave_release_entity call from the SECOND
// one onward -- simulating the resync's own release attempt itself failing (e.g. a
// coordination hiccup unrelated to identity/re-keying, distinct from the rename-specific
// "not found" fixtures elsewhere in this directory). A normal {isError:true} tool result,
// not a JSON-RPC-level throw.

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

const releaseCounts = new Map();

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
      result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fake-weave-mcp-resync-release-fails", version: "0.0.0" } },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    const args = msg.params?.arguments ?? {};
    logCall(toolName);

    if (toolName === "weave_release_entity") {
      const count = (releaseCounts.get(args.entity_name) ?? 0) + 1;
      releaseCounts.set(args.entity_name, count);
      if (count > 1) {
        send({
          jsonrpc: "2.0",
          id: msg.id,
          result: { content: [{ type: "text", text: "weave-mcp: internal error releasing claim" }], isError: true },
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
