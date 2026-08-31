// Item: merge visibility through the code-mode surface. The weave_edit
// layer's MergeStatus (weave-edit-merge-backstop.test.ts) must survive the
// trip into sem.edit()/sem.rename() results, the CallRecord telemetry, and
// deriveApiCallStats -- so a session transcript shows mechanically which
// edits merged over concurrent work, and the model-facing result says so
// in one honest field instead of three bespoke shapes.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { buildSemApi } from "../../src/codemode/api.ts";
import { runInSandbox, SUB_CALLS, type CallRecord } from "../../src/codemode/sandbox.ts";
import { deriveApiCallStats } from "../../src/codemode/tool.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";
import { performRename } from "../../src/tools/internal/rename.ts";
import type { MergeStatus } from "../../src/tools/weave-edit.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE = join(__dirname, "..", "tools", "fixtures", "fake-weave-merge-backstop-server.mjs");

const ALPHA_OLD = "export function alpha(x: number): number {\n  return x + 1;\n}";
const ALPHA_NEW = "export function alpha(x: number): number {\n  return x + 42;\n}";
const BETA = "export function beta(y: number): number {\n  return y * 2;\n}";

function makeRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "api-merge-vis-"));
  writeFileSync(join(dir, "calc.ts"), `${ALPHA_OLD}\n\n${BETA}\n`);
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  return dir;
}

function coordinatorFor(dir: string, mode: string, extraEnv: Record<string, string> = {}): Coordinator {
  return new Coordinator({
    command: process.execPath,
    args: [FAKE],
    env: { FAKE_MERGE_MODE: mode, ...extraEnv },
    cwd: dir,
    agentId: "merge-vis-test-agent",
  });
}

test("sem.edit() result carries merge -- backstop ran, no drift", async () => {
  const dir = makeRepo();
  const coordinator = coordinatorFor(dir, "nodrift");
  try {
    const sem = buildSemApi({ cwd: dir, semBin: "sem", coordinator });
    const r = (await sem.edit({ file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_NEW })) as { merge: MergeStatus };
    assert.deepEqual(r.merge, { attempted: true, performed: false, driftDetected: false });
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("sem.edit() with no coordinator reports the explicit uncoordinated shape, not an absent field", async () => {
  const dir = makeRepo();
  try {
    const sem = buildSemApi({ cwd: dir, semBin: "sem", coordinator: undefined });
    const r = (await sem.edit({ file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_NEW })) as { merge: MergeStatus };
    assert.deepEqual(r.merge, { attempted: false, performed: false });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a performed merge flows into CallRecord.merged via the sandbox, and edits.merged counts it", async () => {
  const dir = makeRepo();
  // "clean" with a no-op substitution: merged_content === ours_content, so
  // the identity guard passes and the merge is reported performed.
  const coordinator = coordinatorFor(dir, "clean", { FAKE_MERGE_SUB: "@@NOPE@@|||@@NOPE@@", FAKE_MERGE_OVER: "beta" });
  try {
    const sem = buildSemApi({ cwd: dir, semBin: "sem", coordinator });
    const result = await runInSandbox(
      `return sem.edit({ file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ${JSON.stringify(ALPHA_NEW)} });`,
      { sem },
    );
    assert.equal(result.ok, true, result.error?.message ?? "");
    assert.deepEqual(result.calls, [{ fn: "edit", ok: true, merged: true }]);
    const stats = deriveApiCallStats(result.calls);
    assert.equal(stats.edits.merged, 1);
    assert.deepEqual(stats.apiCallSequence, ["edit"], "the settled sequence entry shape must not change -- merged is a sibling field, never a suffix");
    const value = result.value as { merge: MergeStatus };
    assert.deepEqual(value.merge, { attempted: true, performed: true, driftDetected: true, mergedOver: ["beta"] });
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("deriveApiCallStats counts merged edits mechanically and leaves apiCallSequence byte-compatible", () => {
  const calls: CallRecord[] = [
    { fn: "grep", ok: true },
    { fn: "edit", ok: true, merged: true },
    { fn: "edit", ok: true },
    { fn: "edit", ok: false, error: "nope" },
  ];
  const stats = deriveApiCallStats(calls);
  assert.deepEqual(stats.edits, { count: 3, refused: 1, merged: 1, reasons: ["nope"] });
  assert.deepEqual(stats.apiCallSequence, ["grep", "edit", "edit", "edit:refused"]);
});

test("sem.rename() aggregates per-entity merges into one MergeStatus and notes it in the rename text", async () => {
  const dir = makeRepo();
  const coordinator = coordinatorFor(dir, "clean", { FAKE_MERGE_SUB: "@@NOPE@@|||@@NOPE@@", FAKE_MERGE_OVER: "beta" });
  try {
    const outcome = await performRename(
      { old_name: "alpha", new_name: "alphaRenamed", claim: true },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);
    const merge = (outcome.details as { merge: MergeStatus }).merge;
    assert.equal(merge.attempted, true);
    assert.equal(merge.performed, true);
    assert.equal(merge.driftDetected, true);
    assert.deepEqual(merge.mergedOver, ["beta"], "own renamed entities are filtered out of the union");
    assert.match(outcome.text, /merged over concurrent changes to beta/);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});
