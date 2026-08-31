// LIVE integration tests for the merge backstop -- same scenarios as the
// deterministic fakes in weave-edit-merge-backstop.test.ts, but against a
// REAL weave-mcp binary, because the contract facts these pin (whole-file
// snapshot pair, NO base_hash, snake_case response keys, conflict as a
// business outcome in an Ok payload, invalid_params on half a snapshot)
// live in the server, and a fake can only ever restate our assumptions
// about them.
//
// Binary resolution: PI_SEM_WEAVE_MCP_BIN, then `weave-mcp` on PATH. When
// neither resolves, each test skips with an explicit reason -- the
// deterministic fakes still cover the client-side logic everywhere.
import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync, spawnSync } from "node:child_process";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";
import { McpClient } from "../../src/bridge/mcp-client.ts";
import { performWeaveEdit, type WeaveEditParams, type MergeStatus } from "../../src/tools/weave-edit.ts";

function resolveWeaveMcp(): string | undefined {
  const fromEnv = process.env.PI_SEM_WEAVE_MCP_BIN;
  if (fromEnv && spawnSync(fromEnv, ["--version"], { stdio: "ignore" }).error === undefined) return fromEnv;
  if (spawnSync("weave-mcp", ["--version"], { stdio: "ignore" }).error === undefined) return "weave-mcp";
  return undefined;
}

const WEAVE_MCP = resolveWeaveMcp();
const SKIP = WEAVE_MCP === undefined ? { skip: "no weave-mcp binary (set PI_SEM_WEAVE_MCP_BIN or put weave-mcp on PATH)" } : {};

const ALPHA_V0 = "export function alpha(x: number): number {\n  return x + 1;\n}";
const ALPHA_OURS = "export function alpha(x: number): number {\n  return x + 42;\n}";
const ALPHA_THEIRS = "export function alpha(x: number): number {\n  return x + 777;\n}";
const BETA_V0 = "export function beta(y: number): number {\n  return y * 2;\n}";
const BETA_THEIRS = "export function beta(y: number): number {\n  return y * 9;\n}";
const fileV0 = `${ALPHA_V0}\n\n${BETA_V0}\n`;

function makeRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "weave-backstop-live-"));
  writeFileSync(join(dir, "calc.ts"), fileV0);
  execFileSync("git", ["init", "-q", "-b", "main"], { cwd: dir });
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  return dir;
}

function liveCoordinator(dir: string, agentId: string): Coordinator {
  return new Coordinator({ command: WEAVE_MCP!, cwd: dir, agentId });
}

test("live: no drift -- base_content matches disk, server stores the update and says drift_detected: false", SKIP, async () => {
  const dir = makeRepo();
  const coordinator = liveCoordinator(dir, "live-agent-nodrift");
  try {
    const ours = fileV0.replace(ALPHA_V0, ALPHA_OURS);
    const outcome = await coordinator.mergeCheck("calc.ts", "main", { name: "alpha" }, ALPHA_OURS, fileV0, ours);
    assert.ok(outcome.ok, JSON.stringify(outcome));
    assert.equal(outcome.drift, false);
    assert.equal(outcome.merged, false);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live: disjoint drift -- the real merge engine reconciles their beta with our alpha and returns merged_content + merged_over", SKIP, async () => {
  const dir = makeRepo();
  const coordinator = liveCoordinator(dir, "live-agent-clean");
  try {
    // Another agent changed beta on disk after we read fileV0.
    writeFileSync(join(dir, "calc.ts"), fileV0.replace(BETA_V0, BETA_THEIRS));
    const ours = fileV0.replace(ALPHA_V0, ALPHA_OURS);
    const outcome = await coordinator.mergeCheck("calc.ts", "main", { name: "alpha" }, ALPHA_OURS, fileV0, ours);
    assert.ok(outcome.ok, JSON.stringify(outcome));
    assert.equal(outcome.drift, true, "snake_case drift_detected must have been parsed from the live response");
    assert.equal(outcome.merged, true);
    assert.ok(outcome.mergedContent?.includes("x + 42"), "our alpha edit survives the merge");
    assert.ok(outcome.mergedContent?.includes("y * 9"), "their beta change is carried into merged_content");
    assert.ok((outcome.mergedOver ?? []).includes("beta"), `merged_over should name beta, got: ${JSON.stringify(outcome.mergedOver)}`);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live: same-entity collision -- an Ok payload carrying merge_conflicts, nothing stored, nothing written", SKIP, async () => {
  const dir = makeRepo();
  const coordinator = liveCoordinator(dir, "live-agent-conflict");
  try {
    writeFileSync(join(dir, "calc.ts"), fileV0.replace(ALPHA_V0, ALPHA_THEIRS));
    const ours = fileV0.replace(ALPHA_V0, ALPHA_OURS);
    const outcome = await coordinator.mergeCheck("calc.ts", "main", { name: "alpha" }, ALPHA_OURS, fileV0, ours);
    assert.ok(!outcome.ok && outcome.conflict, `expected a conflict outcome, got: ${JSON.stringify(outcome).slice(0, 300)}`);
    assert.equal(outcome.conflicts[0]?.entity_name, "alpha");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live: half a snapshot is refused as invalid params -- the pair is one opt-in, and base_hash does not exist", SKIP, async () => {
  const dir = makeRepo();
  const client = new McpClient({ id: "live-half-snapshot", command: WEAVE_MCP!, args: [], cwd: dir, requestTimeoutMs: 15_000 });
  try {
    await client.start();
    await client.callTool("weave_agent_register", { agent_id: "live-agent-half", branch: "main" });
    await assert.rejects(
      client.callTool("weave_update_entity_content", {
        agent_id: "live-agent-half",
        file_path: "calc.ts",
        entity_name: "alpha",
        content: ALPHA_OURS,
        base_content: fileV0,
      }),
      /base_content without ours_content|invalid/i,
      "base_content alone must be a protocol error, not a data-dependent surprise later",
    );
  } finally {
    await client.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("live: performWeaveEdit end-to-end -- claim, gate, write, release against the real server", SKIP, async () => {
  const dir = makeRepo();
  const coordinator = liveCoordinator(dir, "live-agent-e2e");
  try {
    const params: WeaveEditParams = { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_OURS };
    const outcome = await performWeaveEdit(params, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, false, outcome.text);
    const merge = outcome.details.merge as MergeStatus;
    assert.deepEqual(merge, { attempted: true, performed: false, driftDetected: false });
    const coordination = outcome.details.coordination as { claimed: boolean; released?: boolean; updated?: boolean };
    assert.equal(coordination.claimed, true);
    assert.equal(coordination.released, true);
    assert.equal(coordination.updated, true);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});
