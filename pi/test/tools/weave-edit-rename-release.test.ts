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
const FAKE_RENAME_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-rename-server.mjs");

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

// Dogfood round 1, finding 3: a successful rename via
// allow_signature_change orphaned its weave-mcp claim — the release call
// re-resolved the entity by its post-edit name (which, per weave-mcp's own
// apparent behavior, is what the update call re-synced the claim's tracked
// identity to), but our client still queried release by the pre-rename
// name, so weave-mcp reported "entity '<old name>' not found" and the claim
// was never released. The fake fixture reproduces this exact server-side
// behavior: weave_release_entity fails unless entity_name matches whatever
// name weave_update_entity_content's content most recently implied, and
// signals that failure as a JSON-RPC-level protocol error (matching the
// real dogfood evidence's exact "... (code -32602)" wording) rather than an
// {isError: true} tool result — McpClient.callTool rejects on that, so the
// fix must catch and retry around a THROW, not just an isError check.
test("a successful rename releases the weave-mcp claim (not orphaned)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-dogfood-rename-"));
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
      args: [FAKE_RENAME_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "rename-test-agent",
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
      `a successful rename must release its weave-mcp claim, not leave it orphaned. Got: ${outcome.text}`,
    );
    assert.doesNotMatch(outcome.text, /release failed/i, `Got: ${outcome.text}`);

    const details = outcome.details as { coordination?: { released?: boolean; releaseError?: string } };
    assert.equal(details.coordination?.released, true, `coordination.released must be true. Got: ${JSON.stringify(details.coordination)}`);

    // At least one weave_release_entity call must have used the NEW
    // (post-rename) identity — the claim-time identity alone doesn't work
    // against this server, by design of the repro.
    const calls = readCalls(callLog);
    const releaseCalls = calls.filter((c) => c.method === "weave_release_entity");
    assert.ok(
      releaseCalls.some((c) => c.entity_name === "resolveEntityRef"),
      `expected a weave_release_entity call using the renamed entity's identity. Calls: ${JSON.stringify(calls)}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
