#!/usr/bin/env node
// Fake weave-mcp whose CLAIMS ARE SHARED ACROSS PROCESSES via a JSON file
// (FAKE_WEAVE_CLAIMS_FILE env) — each Coordinator spawns its own server
// process, so in-memory state can't model two agents contending for one
// entity. The real weave-mcp shares state through .weave/ in the common
// cwd; this fake shares it through the claims file, which is enough to
// reproduce the one behavior under test: the SECOND claim on an entity
// comes back as an MCP-level SUCCESS whose payload says
// {"result":{"AlreadyClaimed":{"by":"<first agent>"}}} — the exact
// business-outcome-in-Ok-payload shape that made isError-only checking lie.
import { readFileSync, writeFileSync } from "node:fs";

const CLAIMS_FILE = process.env.FAKE_WEAVE_CLAIMS_FILE;
let agentId = "unknown-agent";

function loadClaims() {
  try {
    return JSON.parse(readFileSync(CLAIMS_FILE, "utf8"));
  } catch {
    return {};
  }
}

let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let i = buffer.indexOf("\n");
  while (i !== -1) {
    handleLine(buffer.slice(0, i));
    buffer = buffer.slice(i + 1);
    i = buffer.indexOf("\n");
  }
});

function send(m) {
  process.stdout.write(`${JSON.stringify(m)}\n`);
}

function ok(id, payload) {
  send({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: JSON.stringify(payload) }] } });
}

function handleLine(line) {
  const trimmed = line.trim();
  if (!trimmed) return;
  const msg = JSON.parse(trimmed);
  if (msg.method?.startsWith("notifications/")) return;
  if (msg.method === "initialize") {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: { protocolVersion: "2024-11-05", capabilities: { tools: {} }, serverInfo: { name: "fake-weave-mcp-shared-claims", version: "0.0.0" } },
    });
    return;
  }
  if (msg.method === "tools/call") {
    const tool = msg.params?.name;
    const args = msg.params?.arguments ?? {};
    if (tool === "weave_agent_register") {
      if (typeof args.agent_id === "string") agentId = args.agent_id;
      ok(msg.id, { result: "Registered" });
      return;
    }
    if (tool === "weave_claim_entity") {
      const key = `${args.file_path}::${args.name}`;
      const claims = loadClaims();
      const holder = claims[key];
      if (holder && holder !== agentId) {
        ok(msg.id, { result: { AlreadyClaimed: { by: holder } } });
        return;
      }
      claims[key] = agentId;
      writeFileSync(CLAIMS_FILE, JSON.stringify(claims));
      ok(msg.id, { result: "Claimed", entity_id: `eid-${key}` });
      return;
    }
    if (tool === "weave_release_entity") {
      const key = `${args.file_path}::${args.name}`;
      const claims = loadClaims();
      if (claims[key] === agentId) {
        delete claims[key];
        writeFileSync(CLAIMS_FILE, JSON.stringify(claims));
      }
      ok(msg.id, { result: "Released" });
      return;
    }
    ok(msg.id, { result: "Ok" });
    return;
  }
  send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: `Method not found: ${msg.method}` } });
}
