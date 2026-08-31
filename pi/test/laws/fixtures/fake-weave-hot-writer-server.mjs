#!/usr/bin/env node
// Fake weave-mcp modelling a file that is HOT: on every merge-gate call
// (weave_update_entity_content carrying the base_content/ours_content
// snapshot pair) this server ALSO mutates the target file on disk before
// responding -- a deterministic stand-in for "a concurrent OS process
// lands a write inside the gate's RPC round trip, every single time."
//
// performOneWeaveEdit's pre-write freshness re-check (readFileSync vs the
// `observed` read taken just before the RPC) must then fail on every one
// of its MAX_MERGE_ATTEMPTS passes, driving the engine into its bounded
// write-window-lost refusal -- the only QueueOutcome the deterministic
// fakes in test/tools/ never reach.
//
// Env:
//   FAKE_HOT_FILE  absolute path of the file to mutate on each gate call
//
// Protocol details mirror test/tools/fixtures/fake-weave-merge-backstop-
// server.mjs: gate responses are drift_detected:false Ok payloads (so the
// engine proceeds toward its compare-and-write and fails the compare);
// resync pushes (update WITHOUT the snapshot pair) and claim/release are
// plain successes, logged to FAKE_WEAVE_CALL_LOG when set.
import { appendFileSync, writeFileSync } from "node:fs";

const CALL_LOG = process.env.FAKE_WEAVE_CALL_LOG;
const HOT_FILE = process.env.FAKE_HOT_FILE;
let hotPass = 0;

function logCall(method, note = "") {
  if (CALL_LOG) appendFileSync(CALL_LOG, `${method}${note}\n`);
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
        serverInfo: { name: "fake-weave-hot-writer", version: "0.0.0" },
      },
    });
    return;
  }

  if (msg.method === "tools/call") {
    const toolName = msg.params?.name;
    const args = msg.params?.arguments ?? {};

    if (toolName === "weave_update_entity_content") {
      const hasPair = typeof args.base_content === "string" && typeof args.ours_content === "string";
      if (hasPair) {
        // The hot writer: land a DIFFERENT byte sequence on disk inside
        // every gate round trip, synchronously, before responding -- the
        // caller's `observed` snapshot is stale by the time the verdict
        // arrives, deterministically.
        hotPass += 1;
        if (HOT_FILE) {
          writeFileSync(HOT_FILE, `${args.base_content}\n// hot-writer pass ${hotPass}\n`);
        }
        logCall(toolName, ":gate");
        ok(msg.id, { entity: args.entity_name, drift_detected: false });
        return;
      }
      logCall(toolName, ":resync");
      ok(msg.id, { entity: args.entity_name, content_hash: "fake" });
      return;
    }

    logCall(toolName);
    if (toolName === "weave_claim_entity") {
      ok(msg.id, { result: "Claimed", entity_id: `eid-${args.entity_name}` });
      return;
    }
    ok(msg.id, { result: "ok" });
    return;
  }

  send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `Method not found: ${msg.method}` } });
}
