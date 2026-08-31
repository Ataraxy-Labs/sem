import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_WEAVE_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-server.mjs");

function initGitRepo(dir: string): void {
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"], { cwd: dir });
}

function readCalls(callLog: string): string[] {
  return existsSync(callLog) ? readFileSync(callLog, "utf8").trim().split("\n").filter(Boolean) : [];
}

/**
 * Critical finding: performWeaveEdit calls
 * coordinator.claim() before withFileMutationQueue, but readFile/
 * extractEntities/the rollback writeFile all run inside the queue callback
 * with no try/catch, and coordinator.updateAndRelease only runs after the
 * queue call returns normally. Any throw inside the callback (file deleted
 * between claim and write, sem erroring, a rollback write itself failing,
 * ...) skips the release entirely — the claim is orphaned in weave-mcp
 * forever, blocking every other agent from claiming that entity.
 */
test("a throw inside the file-mutation-queue callback (after a successful claim) still releases the weave-mcp claim", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-lock-leak-"));
  const callLog = join(dir, "calls.log");
  try {
    // Coordination requires a real git repo (Coordinator.claim -> repoRelativePath -> `git rev-parse --show-toplevel`).
    initGitRepo(dir);

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_WEAVE_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "lock-leak-test-agent",
    });

    // The target file does not exist on disk, so readFile() inside the
    // withFileMutationQueue callback throws ENOENT *after* claim() has
    // already succeeded against the fake weave-mcp server.
    const params: WeaveEditParams = {
      file: "does-not-exist.ts",
      entity: { name: "whatever" },
      op: "delete",
    };

    await assert.rejects(
      performWeaveEdit(params, { cwd: dir, semBin: "sem", coordinator, signal: undefined }),
      /ENOENT/,
      "performWeaveEdit should still reject when readFile throws inside the queue callback",
    );

    await coordinator.stop();

    const calls = readCalls(callLog);
    assert.ok(calls.includes("weave_claim_entity"), `expected weave_claim_entity to have been called; got: ${JSON.stringify(calls)}`);
    assert.ok(
      calls.includes("weave_release_entity"),
      `weave_release_entity must be called after the claim succeeded and the queue callback threw — the claim must not be leaked. Calls observed: ${JSON.stringify(calls)}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a successful edit still claims, updates, and releases through the coordinator (regression coverage for the previously-untested path)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-lock-leak-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "hello.ts"), "export function greet(): string {\n  return \"hi\";\n}\n", "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_WEAVE_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "lock-leak-test-agent",
    });

    const outcome = await performWeaveEdit(
      {
        file: "hello.ts",
        entity: { name: "greet" },
        op: "replace",
        content: 'export function greet(): string {\n  return "hi there";\n}',
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);

    await coordinator.stop();

    // Coordinator.ensureRegistered caches the branch it last registered
    // against and skips re-registering for the same branch, so claim() and
    // updateAndRelease() share one weave_agent_register call, not two.
    const calls = readCalls(callLog);
    assert.deepEqual(calls, ["weave_agent_register", "weave_claim_entity", "weave_update_entity_content", "weave_release_entity"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
