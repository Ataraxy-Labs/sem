import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync, existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { performWeaveEdit } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_UPDATE_THROWS_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-update-throws-server.mjs");

function initGitRepo(dir: string): void {
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"], { cwd: dir });
}

function readCalls(callLog: string): Array<{ method: string; entity_name: string | null }> {
  return existsSync(callLog)
    ? readFileSync(callLog, "utf8")
        .trim()
        .split("\n")
        .filter(Boolean)
        .map((line) => JSON.parse(line) as { method: string; entity_name: string | null })
    : [];
}

/**
 * Dogfood round 2, finding 5: the round-1 fix only
 * retried weave_release_entity under the post-edit identity when the
 * claim-time name failed -- it never anticipated that
 * weave_update_entity_content ITSELF can fail the exact same way, for the
 * exact same reason (real weave-mcp resolves entity_name against the
 * file's OWN current content, which pi-sem has already rewritten to disk
 * -- via its own writeFile, independent of weave-mcp -- by the time this
 * call goes out). Confirmed against the real dogfood evidence
 * (results/t3-tools2.jsonl.raw): all 8 identity-changing edits in one
 * batched weave_edit call failed release with a BARE "entity '<claim-time
 * name>' not found" error -- critically, with NO "(also failed under the
 * claim-time identity: ...)" suffix, which updateAndRelease's own combined-
 * failure format always produces when release's primary+retry BOTH run and
 * BOTH fail. The only way to get that bare, single-error shape is for the
 * retry to never have been ATTEMPTED at all -- meaning something upstream
 * of the release retry threw and returned early. weave_update_entity_
 * content's call site had no dedicated try/catch (unlike
 * weave_release_entity's tryRelease, which was built specifically because
 * weave-mcp reports this class of error as a JSON-RPC-level throw, not an
 * {isError:true} result) -- a throw there escaped straight to
 * updateAndRelease's outer catch, returning immediately with just that
 * throw's message, before the primary/retry release logic below it ever
 * ran.
 */
test("a rename's update call failing under the claim-time identity no longer skips release retry (single edit)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-update-throws-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    writeFileSync(
      join(dir, "entities.ts"),
      ["export function resolveEntity(query: string): number {", "  return query.length;", "}", ""].join("\n"),
      "utf8",
    );

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_UPDATE_THROWS_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "update-throws-test-agent",
    });

    const outcome = await performWeaveEdit(
      {
        file: "entities.ts",
        entity: { name: "resolveEntity" },
        op: "replace",
        content: ["export function resolveEntityRef(query: string): number {", "  return query.length;", "}"].join("\n"),
        allow_signature_change: true,
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);

    await coordinator.stop();

    assert.match(
      outcome.text,
      /released/i,
      `the claim must still be released even though the update call failed under the claim-time identity first. Got: ${outcome.text}`,
    );
    assert.doesNotMatch(outcome.text, /release failed/i, `Got: ${outcome.text}`);

    const details = outcome.details as { coordination?: { released?: boolean; releaseError?: string } };
    assert.equal(details.coordination?.released, true, `Got: ${JSON.stringify(details.coordination)}`);

    // Both an update and a release attempt under the renamed identity must
    // actually have been made -- not just a lucky pass.
    const calls = readCalls(callLog);
    assert.ok(
      calls.some((c) => c.method === "weave_update_entity_content" && c.entity_name === "resolveEntityRef"),
      `expected a weave_update_entity_content call under the renamed identity. Calls: ${JSON.stringify(calls)}`,
    );
    assert.ok(
      calls.some((c) => c.method === "weave_release_entity" && c.entity_name === "resolveEntityRef"),
      `expected a weave_release_entity call under the renamed identity. Calls: ${JSON.stringify(calls)}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * The team's literal acceptance bar: "batch of 2 edits where one renames ->
 * both releases succeed (or: result shows zero 'not found' release
 * errors)." Reproduces the exact dogfood shape (a batched edits=[...] call,
 * one entity renamed via allow_signature_change, another left untouched)
 * against the update-throws server.
 */
test("edits= batch: a renaming edit's release succeeds even though its update call failed under the claim-time identity first", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-update-throws-batch-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "a.ts"), "export function resolveEntity(query: string): number {\n  return query.length;\n}\n", "utf8");
    writeFileSync(join(dir, "b.ts"), "export function bar(): number {\n  return 2;\n}\n", "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_UPDATE_THROWS_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "update-throws-batch-test-agent",
    });

    const outcome = await performWeaveEdit(
      {
        edits: [
          {
            file: "a.ts",
            entity: { name: "resolveEntity" },
            op: "replace",
            allow_signature_change: true,
            content: "export function resolveEntityRef(query: string): number {\n  return query.length;\n}",
          },
          { file: "b.ts", entity: { name: "bar" }, op: "replace", content: "export function bar(): number {\n  return 20;\n}" },
        ],
      },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );

    await coordinator.stop();

    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /2\/2 edits applied/);

    // The team's literal bar: zero "not found" release errors anywhere in
    // the batch result text.
    assert.doesNotMatch(outcome.text, /not found/i, `expected zero release errors in the batch result. Got: ${outcome.text}`);
    assert.doesNotMatch(outcome.text, /release failed/i, `Got: ${outcome.text}`);

    const details = outcome.details as { results?: Array<{ details?: { coordination?: { released?: boolean } } }> };
    for (const r of details.results ?? []) {
      assert.equal(r.details?.coordination?.released, true, `every edit in the batch must release cleanly. Got: ${JSON.stringify(details.results)}`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
