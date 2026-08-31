#!/usr/bin/env node
// Fake weave-mcp implementing the merge-backstop contract of the r2 server
// (weave_update_entity_content with an optional base_content + ours_content
// whole-file snapshot pair; NO base_hash anywhere in the protocol), for
// deterministic tests of Coordinator.mergeCheck and performWeaveEdit's
// pre-write gate. The LIVE variant of these scenarios runs against the real
// binary in weave-merge-backstop-live.test.ts; this fake exists so the
// deterministic suite needs no Rust build.
//
// Scenario is chosen by FAKE_MERGE_MODE:
//   nodrift  - respond drift_detected: false (plain stored update)
//   clean    - respond merged: true, drift_detected: true; merged_content is
//              ours_content with FAKE_MERGE_SUB ("old|||new") applied --
//              simulating the disjoint concurrent change merged over;
//              merged_over is FAKE_MERGE_OVER (comma-separated names)
//   conflict - respond the same-entity collision payload: an Ok result whose
//              JSON carries merge_conflicts (business outcome, NOT isError)
//   dropours - respond merged: true but merged_content = base_content (the
//              caller's edit vanished) -- exercises the identity guard
//   strict   - reject any call carrying base_content/ours_content with
//              invalid_params, like a pre-backstop deny_unknown_fields
//              server -- exercises the advisory transport-failure path
//
// Half-snapshot calls (one of base_content/ours_content without the other)
// are always rejected with invalid_params, exactly like the real server.

import { appendFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG;
const MODE = process.env.FAKE_MERGE_MODE ?? "nodrift";

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

function ok(id, payload) {
  send({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: JSON.stringify(payload, null, 2) }] } });
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
        serverInfo: { name: "fake-weave-merge-backstop", version: "0.0.0" },
      },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    const args = msg.params?.arguments ?? {};
    logCall(toolName);

    if (toolName === "weave_claim_entity") {
      ok(msg.id, { result: "Claimed", entity_id: `eid-${args.entity_name}` });
      return;
    }

    if (toolName === "weave_update_entity_content") {
      const hasBase = typeof args.base_content === "string";
      const hasOurs = typeof args.ours_content === "string";
      if (hasBase !== hasOurs) {
        send({
          jsonrpc: "2.0",
          id: msg.id,
          error: { code: -32602, message: "send both base_content and ours_content to enable the concurrent-edit merge backstop, or neither" },
        });
        return;
      }
      if (!hasBase) {
        ok(msg.id, { entity: args.entity_name, content_hash: "fake" });
        return;
      }
      if (MODE === "strict") {
        send({ jsonrpc: "2.0", id: msg.id, error: { code: -32602, message: "unknown field `base_content`" } });
        return;
      }
      if (MODE === "conflict") {
        ok(msg.id, {
          entity: args.entity_name,
          drift_detected: true,
          merge_conflicts: [
            { entity_name: args.entity_name, entity_type: "function", kind: "both_modified", kind_display: "both modified", complexity: "hard" },
          ],
        });
        return;
      }
      if (MODE === "clean" || MODE === "dropours") {
        const [oldText, newText] = (process.env.FAKE_MERGE_SUB ?? "|||").split("|||");
        const merged = MODE === "dropours" ? args.base_content : args.ours_content.split(oldText).join(newText);
        ok(msg.id, {
          entity: args.entity_name,
          drift_detected: true,
          merged: true,
          merged_content: merged,
          merged_over: (process.env.FAKE_MERGE_OVER ?? "").split(",").filter(Boolean),
        });
        return;
      }
      ok(msg.id, { entity: args.entity_name, drift_detected: false });
      return;
    }

    ok(msg.id, { result: "ok" });
    return;
  }

  send({
    jsonrpc: "2.0",
    id: msg.id,
    error: { code: -32601, message: `Method not found: ${msg.method}` },
  });
}
