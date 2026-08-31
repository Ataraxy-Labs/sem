import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { createEditToolDefinition } from "@earendil-works/pi-coding-agent";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SHARED_CLAIMS_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-shared-claims-server.mjs");

/**
 * Phase 1 of the multi-agent same-file campaign: CHARACTERIZE what exists —
 * entity-addressed editing (performWeaveEdit) vs pi's builtin string-match
 * edit tool — under concurrent-writer interleavings, before building any new
 * mechanism. Honest scope notes carried from the original findings:
 *
 *  - Plain oldText matching is CONTENT-addressed, not position-addressed —
 *    an unrelated line-shift or comment insertion does NOT distinguish the
 *    tools (scenarios 1a/3 assert BOTH succeed). The real margin is where
 *    A's oldText overlaps B's change (1b) or the entity's identity itself
 *    changed (4).
 *  - Scenario 4's builtin row is the sharpest: a body-only oldText edit
 *    SILENTLY lands inside an entity that was renamed out from under the
 *    caller — an edit applied to something the author no longer means, with
 *    no error — vs performWeaveEdit's honest not-found refusal.
 *  - Scenario 2 asserts the claim-honesty TRUTH (post claim-fix): a lost
 *    claim reports claimed:false naming the holder, and the disk edit still
 *    lands (advisory-only, documented at the branch in weave-edit.ts).
 *    (In the original this test was born characterizing the misreport and
 *    flipped by the fix; reconstructed directly in its final form.)
 */

const ALPHA = `export function alpha(x: number): number {\n  return x + 1;\n}\n`;
const BETA = `export function beta(y: number): number {\n  return y * 2;\n}\n`;

function makeRepo(): { dir: string; file: string } {
  const dir = mkdtempSync(join(tmpdir(), "phase1-"));
  execFileSync("git", ["init", "-q"], { cwd: dir });
  const file = join(dir, "calc.ts");
  writeFileSync(file, `${BETA}\n${ALPHA}`);
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "base"], { cwd: dir });
  return { dir, file };
}

async function runBuiltinEdit(cwd: string, path: string, oldText: string, newText: string): Promise<{ ok: boolean; error?: string }> {
  const tool = createEditToolDefinition(cwd);
  try {
    await tool.execute("call-1", { path, edits: [{ oldText, newText }] }, undefined, () => {}, { cwd } as never);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : String(err) };
  }
}

const deps = (cwd: string) => ({ cwd, semBin: "sem", coordinator: undefined, signal: undefined });

test("scenario 1a: disjoint entities + line shift — BOTH tools succeed (content addressing is enough here)", async () => {
  const { dir, file } = makeRepo();
  try {
    // A snapshots alpha's exact text before B's edit.
    const aOldText = "  return x + 1;";
    // B rewrites beta, growing it — alpha's line numbers shift.
    const bParams: WeaveEditParams = {
      file: "calc.ts",
      entity: { name: "beta" },
      op: "replace",
      content: "export function beta(y: number): number {\n  const doubled = y * 2;\n  return doubled;\n}",
    };
    const bOutcome = await performWeaveEdit(bParams, deps(dir));
    assert.equal(bOutcome.isError, false, bOutcome.text);

    // A's stale-snapshot edits still land via BOTH routes.
    const builtin = await runBuiltinEdit(dir, file, aOldText, "  return x + 2;");
    assert.equal(builtin.ok, true, `builtin edit is content-addressed; a mere line shift must not break it: ${builtin.error}`);

    const aOutcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: "export function alpha(x: number): number {\n  return x + 3;\n}" },
      deps(dir),
    );
    assert.equal(aOutcome.isError, false, aOutcome.text);
    assert.match(readFileSync(file, "utf8"), /return x \+ 3;/);
    assert.match(readFileSync(file, "utf8"), /const doubled = y \* 2;/, "B's edit survives A's");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scenario 1b: A's oldText spans into B's change — builtin FAILS on mismatch, entity-addressed edit lands", async () => {
  const { dir, file } = makeRepo();
  try {
    // A's oldText includes beta's old body AND the blank line + alpha's
    // opening — context that B is about to rewrite.
    const aOldText = "  return y * 2;\n}\n\nexport function alpha";
    const bOutcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "beta" }, op: "replace", content: "export function beta(y: number): number {\n  return y * 4;\n}" },
      deps(dir),
    );
    assert.equal(bOutcome.isError, false, bOutcome.text);

    const builtin = await runBuiltinEdit(dir, file, aOldText, "  return y * 2;\n}\n\nexport function alphaRenamed");
    assert.equal(builtin.ok, false, "oldText overlapping the concurrent change must fail to match");

    const aOutcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: "export function alpha(x: number): number {\n  return x + 10;\n}" },
      deps(dir),
    );
    assert.equal(aOutcome.isError, false, aOutcome.text);
    assert.match(readFileSync(file, "utf8"), /return x \+ 10;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scenario 2: same entity, concurrent claim — lost claim reports claimed:false naming the holder; the disk edit still lands (advisory)", async () => {
  const { dir, file } = makeRepo();
  const claimsFile = join(dir, "claims.json");
  const mkCoord = (agentId: string) =>
    new Coordinator({
      command: process.execPath,
      args: [SHARED_CLAIMS_SERVER],
      env: { FAKE_WEAVE_CLAIMS_FILE: claimsFile },
      cwd: dir,
      agentId,
    });
  const coordA = mkCoord("agent-A");
  const coordB = mkCoord("agent-B");
  try {
    const aClaim = await coordA.claim("calc.ts", "main", { name: "alpha" });
    assert.equal(aClaim.ok, true, `first claim must win: ${aClaim.ok ? "" : aClaim.reason}`);

    const bClaim = await coordB.claim("calc.ts", "main", { name: "alpha" });
    assert.equal(bClaim.ok, false, "the payload says AlreadyClaimed — isError alone would have called this a win");
    assert.match(bClaim.ok ? "" : bClaim.reason, /already claimed by agent-A/);

    // The full edit pipeline under B's coordinator: honest status, edit lands.
    const outcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: "export function alpha(x: number): number {\n  return x - 1;\n}" },
      { cwd: dir, semBin: "sem", coordinator: coordB, signal: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);
    const coordination = outcome.details.coordination as { claimed: boolean; skippedReason?: string };
    assert.equal(coordination.claimed, false, "a lost claim must never report claimed:true");
    assert.match(coordination.skippedReason ?? "", /already claimed by agent-A/);
    assert.match(readFileSync(file, "utf8"), /return x - 1;/, "advisory-only: the edit proceeds");
  } finally {
    await coordA.stop();
    await coordB.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scenario 3: comment-only shift above the target — BOTH tools still land the edit by name/content", async () => {
  const { dir, file } = makeRepo();
  try {
    writeFileSync(file, `// a new leading comment block\n// that shifts every line below it\n${readFileSync(file, "utf8")}`);
    const builtin = await runBuiltinEdit(dir, file, "  return x + 1;", "  return x + 5;");
    assert.equal(builtin.ok, true, builtin.error ?? "");

    const outcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "beta" }, op: "replace", content: "export function beta(y: number): number {\n  return y * 6;\n}" },
      deps(dir),
    );
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(readFileSync(file, "utf8"), /return y \* 6;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scenario 4: entity renamed underneath a stale caller — entity edit refuses honestly; builtin body-only edit SILENTLY lands inside the renamed entity", async () => {
  const { dir, file } = makeRepo();
  try {
    // B renames beta -> gamma (body kept), out from under A.
    writeFileSync(file, readFileSync(file, "utf8").replace("export function beta(", "export function gamma("));

    // A edits by the stale name: honest refusal, disk untouched.
    const outcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "beta" }, op: "replace", content: "export function beta(y: number): number {\n  return y * 9;\n}" },
      deps(dir),
    );
    assert.equal(outcome.isError, true, "editing a renamed-away entity by its old name must refuse");
    assert.match(outcome.text, /not found|no entity/i);
    assert.doesNotMatch(readFileSync(file, "utf8"), /return y \* 9;/, "the refusal must not have touched disk");

    // A's builtin edit with body-only oldText: lands with NO error, inside
    // an entity the author no longer means. This is the hazard row.
    const builtin = await runBuiltinEdit(dir, file, "  return y * 2;", "  return y * 9;");
    assert.equal(builtin.ok, true, "the silent mislanding: content matched, identity ignored");
    assert.match(readFileSync(file, "utf8"), /export function gamma\(y: number\): number \{\n  return y \* 9;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
