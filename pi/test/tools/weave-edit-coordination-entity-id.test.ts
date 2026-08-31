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
const FAKE_ENTITY_ID_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-entity-id-server.mjs");

function initGitRepo(dir: string): void {
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"], { cwd: dir });
}

function readCalls(callLog: string): Array<{ method: string; entity_name: string | null; entity_id: string | null }> {
  return existsSync(callLog)
    ? readFileSync(callLog, "utf8")
        .trim()
        .split("\n")
        .filter(Boolean)
        .map((line) => JSON.parse(line) as { method: string; entity_name: string | null; entity_id: string | null })
    : [];
}

/**
 * A weave-mcp fix: weave_claim_entity's response can now include
 * `entity_id`, the claim's own stable identity, addressable independent of
 * the entity's current spelling. This is the pi-sem client-side follow-up: `claim()`
 * captures `entity_id` when a connected weave-mcp returns one, and
 * `updateAndRelease` sends it back on the FIRST update/release attempt.
 *
 * The fake server here fails any weave_update_entity_content/
 * weave_release_entity call that lacks a matching entity_id -- exactly how
 * a real un-fixed server fails after a rename (it can only resolve
 * entity_name against the file's now-renamed content). A rename this test
 * drives through therefore proves the client is actually reading and using
 * entity_id, not just tolerating its presence: without it, this edit would
 * fail its release exactly as the older `fake-weave-coordination-rename-
 * server.mjs`-driven tests show a pre-entity_id client failing before its
 * own name-based retry runs.
 */
test("a rename's update and release succeed via the claim's entity_id, addressed under the claim-time name, on the first attempt", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-entity-id-"));
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
      args: [FAKE_ENTITY_ID_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "entity-id-test-agent",
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

    const details = outcome.details as { coordination?: { released?: boolean; releaseError?: string } };
    assert.equal(details.coordination?.released, true, `Got: ${JSON.stringify(details.coordination)}`);

    const calls = readCalls(callLog);

    const updateCalls = calls.filter((c) => c.method === "weave_update_entity_content");
    const releaseCalls = calls.filter((c) => c.method === "weave_release_entity");

    // Exactly one attempt each -- addressed by entity_id on the very first
    // try under the claim-time name, never needing the renamed-identity
    // retry the pre-entity_id client relied on.
    assert.equal(updateCalls.length, 1, `expected exactly one update call, got: ${JSON.stringify(updateCalls)}`);
    assert.equal(releaseCalls.length, 1, `expected exactly one release call, got: ${JSON.stringify(releaseCalls)}`);

    assert.equal(updateCalls[0]!.entity_name, "resolveEntity", "update was addressed under the claim-time name");
    assert.equal(updateCalls[0]!.entity_id, "stable-entity-id-1", "update carried the claim's entity_id");
    assert.equal(releaseCalls[0]!.entity_name, "resolveEntity", "release was addressed under the claim-time name");
    assert.equal(releaseCalls[0]!.entity_id, "stable-entity-id-1", "release carried the claim's entity_id");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * Compatibility: a weave-mcp that never returns entity_id (every other fake
 * server in this directory, and every currently-shipping real weave-mcp)
 * must see behavior byte-for-byte unchanged -- no entity_id field sent at
 * all, so an older `deny_unknown_fields` server is never put at risk.
 */
test("against a server that never returns entity_id, no entity_id field is ever sent", async () => {
  const FAKE_LEGACY_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-server.mjs");
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-entity-id-legacy-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_LEGACY_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "entity-id-legacy-test-agent",
    });

    const outcome = await performWeaveEdit(
      { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);
    await coordinator.stop();

    // fake-weave-coordination-server.mjs doesn't log calls in the
    // {method, entity_name, entity_id} shape this file's readCalls expects
    // -- so read the raw call log directly and just prove entity_id never
    // appears anywhere in it, regardless of that fixture's own log format.
    const raw = existsSync(callLog) ? readFileSync(callLog, "utf8") : "";
    assert.doesNotMatch(raw, /entity_id/, `no entity_id field should ever be sent to a server that never advertised one. Got call log: ${raw}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
