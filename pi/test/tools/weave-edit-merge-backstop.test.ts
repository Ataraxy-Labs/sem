import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { performWeaveEdit, type WeaveEditParams, type MergeStatus } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE = join(__dirname, "fixtures", "fake-weave-merge-backstop-server.mjs");

const ALPHA_OLD = "export function alpha(x: number): number {\n  return x + 1;\n}";
const ALPHA_NEW = "export function alpha(x: number): number {\n  return x + 42;\n}";
const BETA_OLD = "export function beta(y: number): number {\n  return y * 2;\n}";
const BETA_THEIRS = "export function beta(y: number): number {\n  return y * 9;\n}";
const CALC = `${ALPHA_OLD}\n\n${BETA_OLD}\n`;

function makeRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "weave-merge-backstop-"));
  writeFileSync(join(dir, "calc.ts"), CALC);
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  return dir;
}

function coordinatorFor(dir: string, mode: string, callLog: string, extraEnv: Record<string, string> = {}): Coordinator {
  return new Coordinator({
    command: process.execPath,
    args: [FAKE],
    env: { FAKE_MERGE_MODE: mode, FAKE_WEAVE_CALL_LOG: callLog, ...extraEnv },
    cwd: dir,
    agentId: "merge-backstop-test-agent",
  });
}

function readCalls(callLog: string): string[] {
  return existsSync(callLog) ? readFileSync(callLog, "utf8").trim().split("\n").filter(Boolean) : [];
}

const EDIT_ALPHA: WeaveEditParams = {
  file: "calc.ts",
  entity: { name: "alpha" },
  op: "replace",
  content: ALPHA_NEW,
};

test("no drift: the gate stores the update once (no legacy double push) and reports driftDetected: false", async () => {
  const dir = makeRepo();
  const callLog = join(dir, "calls.log");
  const coordinator = coordinatorFor(dir, "nodrift", callLog);
  try {
    const outcome = await performWeaveEdit(EDIT_ALPHA, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, false, outcome.text);
    const merge = outcome.details.merge as MergeStatus;
    assert.deepEqual(merge, { attempted: true, performed: false, driftDetected: false });
    assert.ok(readFileSync(join(dir, "calc.ts"), "utf8").includes("x + 42"));
    const updates = readCalls(callLog).filter((c) => c === "weave_update_entity_content");
    assert.equal(updates.length, 1, "the gate call IS the CRDT update -- no second legacy push");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("clean disjoint merge: the caller writes merged_content (their beta + our alpha), and both text and details say so", async () => {
  const dir = makeRepo();
  const callLog = join(dir, "calls.log");
  // The fake merges "theirs" in by rewriting beta inside ours_content --
  // deterministic stand-in for the real engine's disjoint-entity merge.
  const coordinator = coordinatorFor(dir, "clean", callLog, {
    FAKE_MERGE_SUB: `${BETA_OLD}|||${BETA_THEIRS}`,
    FAKE_MERGE_OVER: "beta",
  });
  try {
    const outcome = await performWeaveEdit(EDIT_ALPHA, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, false, outcome.text);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.ok(disk.includes("x + 42"), "our alpha edit landed");
    assert.ok(disk.includes("y * 9"), "their concurrent beta change was merged over, not clobbered");
    const merge = outcome.details.merge as MergeStatus;
    assert.deepEqual(merge, { attempted: true, performed: true, driftDetected: true, mergedOver: ["beta"] });
    assert.match(outcome.text, /Merged over concurrent changes to beta/);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("same-entity collision: the edit is refused, nothing is written, and the claim is still released", async () => {
  const dir = makeRepo();
  const callLog = join(dir, "calls.log");
  const coordinator = coordinatorFor(dir, "conflict", callLog);
  try {
    const outcome = await performWeaveEdit(EDIT_ALPHA, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /refused.*collides|collides.*refused/s);
    assert.match(outcome.text, /Nothing was written/);
    assert.equal(readFileSync(join(dir, "calc.ts"), "utf8"), CALC, "disk untouched on a merge conflict");
    const conflicts = outcome.details.mergeConflicts as Array<{ entity_name?: string }>;
    assert.equal(conflicts[0]?.entity_name, "alpha");
    const merge = outcome.details.merge as MergeStatus;
    assert.deepEqual(merge, { attempted: true, performed: false, driftDetected: true });
    assert.ok(readCalls(callLog).includes("weave_release_entity"), "the claim must still be released after a refusal");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("identity guard: a merged output that dropped our edit is refused instead of silently written", async () => {
  const dir = makeRepo();
  const callLog = join(dir, "calls.log");
  const coordinator = coordinatorFor(dir, "dropours", callLog);
  try {
    const outcome = await performWeaveEdit(EDIT_ALPHA, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no longer contains this edit's own text/);
    assert.equal(readFileSync(join(dir, "calc.ts"), "utf8"), CALC, "disk untouched when the merge dropped ours");
    // The gate had already stored our content in the CRDT before the guard
    // refused -- a resync push must follow so the CRDT matches disk again.
    const updates = readCalls(callLog).filter((c) => c === "weave_update_entity_content");
    assert.equal(updates.length, 2, "gate update + resync of the restored entity text");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pre-backstop strict server: the gate call is rejected, the edit proceeds advisory-style with the legacy push", async () => {
  const dir = makeRepo();
  const callLog = join(dir, "calls.log");
  const coordinator = coordinatorFor(dir, "strict", callLog);
  try {
    const outcome = await performWeaveEdit(EDIT_ALPHA, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, false, outcome.text);
    assert.ok(readFileSync(join(dir, "calc.ts"), "utf8").includes("x + 42"), "the edit still lands when the backstop is unavailable");
    const merge = outcome.details.merge as MergeStatus;
    assert.deepEqual(merge, { attempted: true, performed: false }, "attempted but no verdict -- and no invented driftDetected");
    const updates = readCalls(callLog).filter((c) => c === "weave_update_entity_content");
    assert.equal(updates.length, 2, "rejected gate call + legacy snapshot-free push");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("claim=false skips the backstop entirely and reports the explicit uncoordinated shape", async () => {
  const dir = makeRepo();
  const callLog = join(dir, "calls.log");
  const coordinator = coordinatorFor(dir, "nodrift", callLog);
  try {
    const outcome = await performWeaveEdit({ ...EDIT_ALPHA, claim: false }, { cwd: dir, semBin: "sem", coordinator, signal: undefined });
    assert.equal(outcome.isError, false, outcome.text);
    const merge = outcome.details.merge as MergeStatus;
    assert.deepEqual(merge, { attempted: false, performed: false });
    assert.equal(readCalls(callLog).length, 0, "claim=false is a full coordination opt-out -- no server traffic at all");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});
