#!/usr/bin/env node
// Fake weave-mcp coordination server reproducing the dogfood round-2
// finding: real weave-mcp resolves entity_name against the
// FILE'S OWN CURRENT content, not just the claim's own bookkeeping. pi-sem
// writes an edit's new content to disk BEFORE calling weave_update_entity_
// content, so by the time that call reaches weave-mcp with the CLAIM-TIME
// (pre-rename) entity_name, the old name genuinely no longer resolves in
// the file -- weave_update_entity_content itself throws "not found" under
// the stale name, not just weave_release_entity (which is all the round-1
// fix, e466161, accounted for). Only a query naming whatever the just-
// pushed content actually implies succeeds.
//
// extractEntityName recognizes both a `fn`/`function` declaration (Rust/TS)
// and a `test("description", ...)` block (test-type entities) -- the exact
// two shapes the round-2 dogfood repro exercised (a function rename plus 7
// renamed node:test blocks in the same batch).

import { appendFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG;

function logCall(method, args) {
  if (CALL_LOG) appendFileSync(CALL_LOG, `${JSON.stringify({ method, entity_name: args?.entity_name ?? null })}\n`);
}

function extractEntityName(content) {
  const fn = /(?:fn|function)\s+([A-Za-z_][A-Za-z0-9_]*)/.exec(content ?? "");
  if (fn) return fn[1];
  const testName = /test\(\s*"([^"]+)"/.exec(content ?? "");
  if (testName) return testName[1];
  return null;
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

// weave-mcp's own tracked identity for the current claim -- starts as
// whatever was claimed, shifts to whichever name a SUCCESSFUL
// weave_update_entity_content call was accepted under.
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
      result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fake-weave-mcp-update-throws", version: "0.0.0" } },
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
      const impliedName = extractEntityName(args.content);
      // Only a query naming whatever the pushed content itself implies is
      // accepted -- the claim-time name fails once the rename has already
      // landed on disk, same as weave_release_entity below.
      if (impliedName && args.entity_name !== impliedName) {
        send({
          jsonrpc: "2.0",
          id: msg.id,
          error: { code: -32602, message: `entity '${args.entity_name}' not found in '${args.file_path}'` },
        });
        return;
      }
      trackedName = args.entity_name;
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
