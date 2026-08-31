import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { performWeaveEdit } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_WEAVE_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-server.mjs");
const FAKE_RECLAIM_FAILS_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-reclaim-fails-server.mjs");
const FAKE_RESYNC_RELEASE_FAILS_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-resync-release-fails-server.mjs");

function initGitRepo(dir: string): void {
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"], { cwd: dir });
}

function readCalls(callLog: string): string[] {
  return existsSync(callLog) ? readFileSync(callLog, "utf8").trim().split("\n").filter(Boolean) : [];
}

function countCalls(calls: string[], method: string): number {
  return calls.filter((c) => c === method).length;
}

/**
 * performWeaveEditBatch's
 * atomic=true restores every touched file to its pre-batch snapshot when a
 * later edit fails, but an EARLIER edit in the same batch that had already
 * succeeded already ran its own full claim/update/release cycle against
 * weave-mcp before the failure was even hit — its update pushed the
 * (now-undone) post-edit content, and release already let the claim go.
 * The disk rollback alone leaves weave-mcp's tracked content for that
 * entity still saying the reverted edit landed, with no active claim
 * standing between the two states ever getting resolved.
 */
test("atomic rollback re-syncs weave-mcp coordination for edits that had already succeeded before the batch failed", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-resync-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    const originalA = "export function foo(): number {\n  return 1;\n}\n";
    const originalB = "export function bar(): number {\n  return 2;\n}\n";
    writeFileSync(join(dir, "a.ts"), originalA, "utf8");
    writeFileSync(join(dir, "b.ts"), originalB, "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_WEAVE_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "resync-test-agent",
    });

    const outcome = await performWeaveEdit(
      {
        atomic: true,
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "totallyMissingName" }, op: "replace", content: "whatever" },
        ],
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );

    await coordinator.stop();

    assert.equal(outcome.isError, true, "an atomic batch that had to roll back must report failure");
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), originalA, "a.ts must be rolled back to its pre-batch content on disk");

    const calls = readCalls(callLog);
    // a.ts#foo's edit already ran ONE full claim/update/release cycle for
    // the (now-undone) edit itself -- the rollback must run a SECOND full
    // cycle to push a.ts#foo's TRUE, restored content back to weave-mcp,
    // not leave the first cycle's now-false content standing uncorrected.
    assert.ok(
      countCalls(calls, "weave_claim_entity") >= 2,
      `expected at least 2 weave_claim_entity calls (original edit + rollback resync), got ${countCalls(calls, "weave_claim_entity")}. Calls: ${JSON.stringify(calls)}`,
    );
    assert.ok(
      countCalls(calls, "weave_update_entity_content") >= 2,
      `expected at least 2 weave_update_entity_content calls (original edit + rollback resync), got ${countCalls(calls, "weave_update_entity_content")}. Calls: ${JSON.stringify(calls)}`,
    );
    assert.ok(
      countCalls(calls, "weave_release_entity") >= 2,
      `expected at least 2 weave_release_entity calls (original edit + rollback resync), got ${countCalls(calls, "weave_release_entity")}. Calls: ${JSON.stringify(calls)}`,
    );

    const details = outcome.details as {
      coordinationResync?: Array<{ file: string; entity: { name: string }; resynced: boolean }>;
    };
    assert.ok(
      details.coordinationResync?.some((n) => n.file === "a.ts" && n.entity.name === "foo" && n.resynced === true),
      `expected a successfully-resynced coordinationResync entry for a.ts#foo. Got: ${JSON.stringify(details.coordinationResync)}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// b.ts's own edit never succeeded (its entity was never found), so it was
// never claimed in the first place -- nothing to resync for it, and the
// resync pass must not invent a claim/release cycle for an entity that was
// never coordinated at all.
test("atomic rollback resync only touches entities whose edit had actually been coordinated", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-resync-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");
    writeFileSync(join(dir, "b.ts"), "export function bar(): number {\n  return 2;\n}\n", "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_WEAVE_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "resync-test-agent-2",
    });

    const outcome = await performWeaveEdit(
      {
        atomic: true,
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "totallyMissingName" }, op: "replace", content: "whatever" },
        ],
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    await coordinator.stop();

    const details = outcome.details as { coordinationResync?: Array<{ file: string; entity: { name: string } }> };
    assert.ok(
      !details.coordinationResync?.some((n) => n.file === "b.ts"),
      `b.ts's never-succeeded edit must not appear in coordinationResync. Got: ${JSON.stringify(details.coordinationResync)}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Review pass 4, finding #6: resyncCoordinationAfterRollback's fallback
// branches (`!claimResult.ok` and `!releaseResult.ok`) were implemented but
// never exercised by a test -- both are now driven directly with dedicated
// fake servers, rather than only ever seeing the happy path.
test("atomic rollback resync reports resynced:false with a reason when re-claiming loses a race to another agent", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-resync-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    const originalA = "export function foo(): number {\n  return 1;\n}\n";
    writeFileSync(join(dir, "a.ts"), originalA, "utf8");
    writeFileSync(join(dir, "b.ts"), "export function bar(): number {\n  return 2;\n}\n", "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_RECLAIM_FAILS_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "resync-reclaim-fails-agent",
    });

    const outcome = await performWeaveEdit(
      {
        atomic: true,
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "totallyMissingName" }, op: "replace", content: "whatever" },
        ],
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    await coordinator.stop();

    assert.equal(outcome.isError, true, "atomic rollback must still report failure");
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), originalA, "a.ts must still be rolled back on disk regardless of resync's own outcome");

    const details = outcome.details as {
      coordinationResync?: Array<{ file: string; entity: { name: string }; resynced: boolean; reason?: string }>;
    };
    const note = details.coordinationResync?.find((n) => n.file === "a.ts" && n.entity.name === "foo");
    assert.ok(note, `expected a coordinationResync entry for a.ts#foo. Got: ${JSON.stringify(details.coordinationResync)}`);
    assert.equal(note!.resynced, false, "must never claim success when the re-claim itself failed");
    assert.match(
      note!.reason ?? "",
      /could not re-claim to resync.*already claimed by another agent/,
      `reason must explain the re-claim failure, not swallow it. Got: ${JSON.stringify(note)}`,
    );

    // The result text surfaces this too, not just details -- the model
    // reading only the text must still learn coordination is stale here.
    assert.match(outcome.text, /could not be fully resynced.*a\.ts#foo/s);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("atomic rollback resync reports resynced:false with a reason when the resync's own release call fails", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-resync-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    const originalA = "export function foo(): number {\n  return 1;\n}\n";
    writeFileSync(join(dir, "a.ts"), originalA, "utf8");
    writeFileSync(join(dir, "b.ts"), "export function bar(): number {\n  return 2;\n}\n", "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_RESYNC_RELEASE_FAILS_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "resync-release-fails-agent",
    });

    const outcome = await performWeaveEdit(
      {
        atomic: true,
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "totallyMissingName" }, op: "replace", content: "whatever" },
        ],
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    await coordinator.stop();

    assert.equal(outcome.isError, true, "atomic rollback must still report failure");
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), originalA, "a.ts must still be rolled back on disk regardless of resync's own outcome");

    const details = outcome.details as {
      coordinationResync?: Array<{ file: string; entity: { name: string }; resynced: boolean; reason?: string }>;
    };
    const note = details.coordinationResync?.find((n) => n.file === "a.ts" && n.entity.name === "foo");
    assert.ok(note, `expected a coordinationResync entry for a.ts#foo. Got: ${JSON.stringify(details.coordinationResync)}`);
    assert.equal(note!.resynced, false, "must never claim success when the resync's own release failed");
    assert.match(
      note!.reason ?? "",
      /failed to push restored content.*internal error releasing claim/,
      `reason must explain the release failure, not swallow it. Got: ${JSON.stringify(note)}`,
    );

    // The re-claim itself must have succeeded here (this fixture only fails
    // release) -- confirms the release failure is being caught on its own
    // terms, not as a side effect of a re-claim that never happened.
    const calls = readCalls(callLog);
    assert.ok(
      countCalls(calls, "weave_claim_entity") >= 2,
      `expected the resync's re-claim to have been attempted (and succeeded) before release failed. Calls: ${JSON.stringify(calls)}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
