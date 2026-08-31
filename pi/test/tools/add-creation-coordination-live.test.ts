import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSemApi, createChangeLog, type AddResult } from "../../src/codemode/api.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

/**
 * Creation under coordination, against the REAL weave-mcp: sem.add()
 * registers the new file's first entity with live coordination
 * (claim -> update -> release), so
 *   1. the creation is VISIBLE -- registered:true with the entity_id the
 *      server minted, proven end to end, and
 *   2. a second agent creating the same module is never a silent
 *      last-writer-wins: sequentially it is refused by the exists check
 *      (pinned here, with the refusal pointing at sem.edit -- the honest
 *      "someone was here first" signal).
 *
 * The DISCLOSED limit this file deliberately does NOT paper over: the
 * merge backstop cannot gate creation BEFORE the write, because weave-mcp
 * resolves both the file and the entity query against DISK -- probed
 * against the real server, a brand-new path errors with "failed to read".
 * So the racing check-to-write window is still last-writer-wins until the
 * server can treat a missing file as an empty base; that gap is
 * documented on AddResult.coordination, not silently absorbed here.
 */

function resolveWeaveMcp(): string | undefined {
  const fromEnv = process.env.PI_SEM_WEAVE_MCP_BIN;
  if (fromEnv && spawnSync(fromEnv, ["--version"], { stdio: "ignore" }).error === undefined) return fromEnv;
  if (spawnSync("weave-mcp", ["--version"], { stdio: "ignore" }).error === undefined) return "weave-mcp";
  return undefined;
}

const WEAVE_MCP = resolveWeaveMcp();
const SKIP = WEAVE_MCP === undefined ? { skip: "no weave-mcp binary (set PI_SEM_WEAVE_MCP_BIN or put weave-mcp on PATH)" } : {};

function makeCrate(): string {
  const dir = mkdtempSync(join(tmpdir(), "add-coord-live-"));
  writeFileSync(join(dir, "Cargo.toml"), '[package]\nname = "fixture"\nversion = "0.0.0"\nedition = "2021"\n');
  mkdirSync(join(dir, "src"));
  writeFileSync(join(dir, "src", "lib.rs"), "pub fn seed() -> u32 { 0 }\n");
  execFileSync("git", ["init", "-q", "-b", "main"], { cwd: dir });
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  return dir;
}

test("sem.add registers the created entity with live coordination (claim/update/release, real server)", SKIP, async () => {
  const dir = makeCrate();
  const coordinator = new Coordinator({ command: WEAVE_MCP!, cwd: dir, agentId: "creator-A" });
  try {
    const changes = createChangeLog();
    const sem = buildSemApi({ cwd: dir, semBin: "sem", coordinator, changes });
    const result = (await sem.add({ module: "checksum", content: "pub fn checksum(x: u32) -> u32 { x ^ 0x9e37 }\n" })) as AddResult;
    assert.equal(result.created, true);
    assert.ok(result.coordination, "a configured coordinator must produce a coordination report");
    assert.equal(result.coordination!.registered, true, JSON.stringify(result.coordination));
    assert.equal(result.coordination!.entity, "checksum");
    assert.match(result.coordination!.entityId ?? "", /checksum/, "the server-minted entity_id names the entity");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("second creator of the same module is refused honestly, never a silent overwrite (sequential)", SKIP, async () => {
  const dir = makeCrate();
  const coordA = new Coordinator({ command: WEAVE_MCP!, cwd: dir, agentId: "creator-A" });
  const coordB = new Coordinator({ command: WEAVE_MCP!, cwd: dir, agentId: "creator-B" });
  try {
    const semA = buildSemApi({ cwd: dir, semBin: "sem", coordinator: coordA, changes: createChangeLog() });
    const semB = buildSemApi({ cwd: dir, semBin: "sem", coordinator: coordB, changes: createChangeLog() });
    const first = (await semA.add({ module: "checksum", content: "pub fn checksum(x: u32) -> u32 { x }\n" })) as AddResult;
    assert.equal(first.coordination?.registered, true);
    await assert.rejects(semB.add({ module: "checksum", content: "pub fn checksum(x: u32) -> u32 { x + 1 }\n" }), (err: Error) => {
      assert.match(err.message, /already exists/, "the second creator must be told someone was here first");
      assert.match(err.message, /sem\.edit/, "and pointed at the entity-level path forward");
      return true;
    });
  } finally {
    await coordA.stop();
    await coordB.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("registration failure is advisory: a dead coordinator reports registered:false, the file still lands", async () => {
  const dir = makeCrate();
  const dead = new Coordinator({ command: "/nonexistent/weave-mcp-binary", cwd: dir, agentId: "creator-A" });
  try {
    const sem = buildSemApi({ cwd: dir, semBin: "sem", coordinator: dead, changes: createChangeLog() });
    const result = (await sem.add({ module: "checksum", content: "pub fn checksum(x: u32) -> u32 { x }\n" })) as AddResult;
    assert.equal(result.created, true, "the write must never be unwound by a coordination failure");
    assert.equal(result.coordination?.registered, false);
    assert.ok((result.coordination?.reason ?? "").length > 0, "the failure carries its reason");
  } finally {
    await dead.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});
