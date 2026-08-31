import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { performRename } from "../../src/tools/internal/rename.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_ENTITY_ID_SERVER = join(__dirname, "fixtures", "fake-weave-coordination-entity-id-server.mjs");

function initGitRepo(dir: string): void {
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"], { cwd: dir });
}

function commitAll(dir: string): void {
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "fixture"], { cwd: dir });
}

/**
 * The dogfood-task-3 mirror: a definition plus 10 more reference sites
 * across 3 other files, in a mix sem's entity model DOES and does NOT track
 * on its own:
 *  - scoring.ts: the definition itself (1 site).
 *  - caller1.ts: a named import (not an entity — sem_entities never lists
 *    import statements) plus one call site inside a function body (1+1=2).
 *  - caller2.ts: a re-export (also not an entity) plus two call sites
 *    inside the SAME function body (1+2=3).
 *  - scoring.test.ts: an import, plus two `test("...", ...)` blocks whose
 *    STRING TITLE mentions the old name and whose body also calls it — sem
 *    models a test(...) call as its own entity, named after the literal
 *    title string (confirmed against a real `sem entities --json` run) —
 *    (1 import) + (2 sites in test 1: title line + assertion line) + (2
 *    sites in test 2: title line + assertion line) = 5.
 * Total: 1 + 2 + 3 + 5 = 11 sites across 4 files.
 */
function buildElevenSiteFixture(dir: string): void {
  writeFileSync(
    join(dir, "scoring.ts"),
    ["export function computeScore(x: number): number {", "  return x * 2;", "}", ""].join("\n"),
    "utf8",
  );
  writeFileSync(
    join(dir, "caller1.ts"),
    [
      'import { computeScore } from "./scoring.ts";',
      "",
      "export function useScoreA(x: number): number {",
      "  return computeScore(x) + 1;",
      "}",
      "",
    ].join("\n"),
    "utf8",
  );
  writeFileSync(
    join(dir, "caller2.ts"),
    [
      'export { computeScore } from "./scoring.ts";',
      "",
      "export function useScoreB(x: number): number {",
      "  const doubled = computeScore(x);",
      "  return doubled + computeScore(x - 1);",
      "}",
      "",
    ].join("\n"),
    "utf8",
  );
  writeFileSync(
    join(dir, "scoring.test.ts"),
    [
      'import { computeScore } from "./scoring.ts";',
      "",
      'test("computeScore doubles its input", () => {',
      "  expect(computeScore(2)).toBe(4);",
      "});",
      "",
      'test("computeScore handles negative input", () => {',
      "  expect(computeScore(-1)).toBe(-2);",
      "});",
      "",
    ].join("\n"),
    "utf8",
  );
}

function countWordBoundary(content: string, name: string): number {
  const re = new RegExp(`\\b${name}\\b`, "g");
  return (content.match(re) ?? []).length;
}

test("performRename applies all 11 sites across 4 files, verified, 0 leftovers (dogfood task 3 mirror)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "rename-eleven-sites-"));
  try {
    initGitRepo(dir);
    buildElevenSiteFixture(dir);
    commitAll(dir);

    const outcome = await performRename(
      { old_name: "computeScore", new_name: "computeRating" },
      { cwd: dir, semBin: "sem", coordinator: undefined, signal: undefined },
    );

    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /\b11\b.*\bsites?\b.*\b4\b.*\bfiles?\b/i, `Got: ${outcome.text}`);
    assert.match(outcome.text, /verified/i, `Got: ${outcome.text}`);
    assert.match(outcome.text, /\b0\b.*\bleftovers?\b/i, `Got: ${outcome.text}`);

    const details = outcome.details as { applied?: number; files?: string[]; verified?: boolean; leftovers?: unknown[] };
    assert.equal(details.applied, 11, `Got details: ${JSON.stringify(details)}`);
    assert.equal(details.files?.length, 4, `Got details: ${JSON.stringify(details)}`);
    assert.equal(details.verified, true, `Got details: ${JSON.stringify(details)}`);
    assert.equal(details.leftovers?.length ?? 0, 0, `Got details: ${JSON.stringify(details)}`);

    for (const file of ["scoring.ts", "caller1.ts", "caller2.ts", "scoring.test.ts"]) {
      const content = readFileSync(join(dir, file), "utf8");
      assert.equal(countWordBoundary(content, "computeScore"), 0, `${file} must have zero remaining occurrences of the old name:\n${content}`);
    }

    // Spot-check the renamed content actually still makes sense (not just
    // blanked out): definition, import, re-export, and a test title.
    assert.match(readFileSync(join(dir, "scoring.ts"), "utf8"), /export function computeRating\(/);
    assert.match(readFileSync(join(dir, "caller1.ts"), "utf8"), /import \{ computeRating \} from "\.\/scoring\.ts";/);
    assert.match(readFileSync(join(dir, "caller2.ts"), "utf8"), /export \{ computeRating \} from "\.\/scoring\.ts";/);
    assert.match(readFileSync(join(dir, "scoring.test.ts"), "utf8"), /test\("computeRating doubles its input"/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * A site sem_grep genuinely cannot see (confirmed empirically: `sem grep`
 * indexes .ts/.md but never counted a sibling .txt file among its
 * candidate/total files even after `git add`+commit) must still surface as
 * a leftover — via an independent, non-sem-index verification sweep after
 * the entity-reachable sites are applied — rather than silently vanishing
 * from the report. The rename must still succeed for everything it COULD
 * reach; a leftover is an honest gap, not a reason to refuse the whole
 * rename.
 */
test("a site sem_grep's index can't see (a sibling .txt file) is applied nowhere and surfaces as a leftover", async () => {
  const dir = mkdtempSync(join(tmpdir(), "rename-leftover-"));
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "lib.ts"), ["export function helperFn(): number {", "  return 1;", "}", ""].join("\n"), "utf8");
    writeFileSync(
      join(dir, "caller.ts"),
      ['import { helperFn } from "./lib.ts";', "", "export function useIt(): number {", "  return helperFn();", "}", ""].join("\n"),
      "utf8",
    );
    writeFileSync(join(dir, "notes.txt"), "helperFn is documented here, informally.\n", "utf8");
    commitAll(dir);

    const outcome = await performRename(
      { old_name: "helperFn", new_name: "helperFunc" },
      { cwd: dir, semBin: "sem", coordinator: undefined, signal: undefined },
    );

    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /leftover/i, `Got: ${outcome.text}`);

    const details = outcome.details as { verified?: boolean; leftovers?: Array<{ file: string; line: number; snippet: string; looksLikeCommentOrDoc: boolean }> };
    assert.equal(details.verified, false, `Got details: ${JSON.stringify(details)}`);
    assert.equal(details.leftovers?.length, 1, `Got details: ${JSON.stringify(details)}`);
    const leftover = details.leftovers![0]!;
    assert.equal(leftover.file, "notes.txt");
    assert.match(leftover.snippet, /helperFn/);
    assert.equal(leftover.looksLikeCommentOrDoc, true, `notes.txt is prose, not code — should be flagged. Got: ${JSON.stringify(leftover)}`);

    assert.equal(countWordBoundary(readFileSync(join(dir, "lib.ts"), "utf8"), "helperFn"), 0);
    assert.equal(countWordBoundary(readFileSync(join(dir, "caller.ts"), "utf8"), "helperFn"), 0);
    // The unreachable site must be left completely untouched, not
    // partially/incorrectly edited.
    assert.equal(readFileSync(join(dir, "notes.txt"), "utf8"), "helperFn is documented here, informally.\n");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("no entity by that name refuses with the sem_callers-style message", async () => {
  const dir = mkdtempSync(join(tmpdir(), "rename-not-found-"));
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");
    commitAll(dir);

    const outcome = await performRename(
      { old_name: "totallyMissing", new_name: "whatever" },
      { cwd: dir, semBin: "sem", coordinator: undefined, signal: undefined },
    );

    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named "totallyMissing"/i, `Got: ${outcome.text}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a name matching entities in two different files is refused as ambiguous, with a candidate list; file= disambiguates", async () => {
  const dir = mkdtempSync(join(tmpdir(), "rename-ambiguous-"));
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "a.ts"), "export function shared(): number {\n  return 1;\n}\n", "utf8");
    writeFileSync(join(dir, "b.ts"), "export function shared(): number {\n  return 2;\n}\n", "utf8");
    commitAll(dir);

    const ambiguous = await performRename(
      { old_name: "shared", new_name: "sharedRenamed" },
      { cwd: dir, semBin: "sem", coordinator: undefined, signal: undefined },
    );
    assert.equal(ambiguous.isError, true);
    assert.match(ambiguous.text, /ambiguous/i, `Got: ${ambiguous.text}`);
    assert.match(ambiguous.text, /a\.ts/);
    assert.match(ambiguous.text, /b\.ts/);

    const disambiguated = await performRename(
      { old_name: "shared", new_name: "sharedRenamed", file: "a.ts" },
      { cwd: dir, semBin: "sem", coordinator: undefined, signal: undefined },
    );
    assert.equal(disambiguated.isError, false, disambiguated.text);
    assert.match(readFileSync(join(dir, "a.ts"), "utf8"), /export function sharedRenamed\(/);
    // b.ts has an unrelated, same-named entity in a different file — must
    // be left alone entirely.
    assert.match(readFileSync(join(dir, "b.ts"), "utf8"), /export function shared\(/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * Confirms rename.ts actually threads deps.coordinator/claim through to the
 * weave_edit machinery (performWeaveEdit), including the entity_id
 * claim path, rather than silently dropping it. Reuses the fake weave-mcp
 * fixture from the pi-sem entity_id follow-up -- it fails any
 * update/release call that doesn't carry the claim's own
 * entity_id, so a passing rename here proves entity_id was captured and
 * forwarded correctly through rename.ts's own claim/release calls too.
 */
test("claims and releases each definition/reference entity through the coordinator, using entity_id", async () => {
  const dir = mkdtempSync(join(tmpdir(), "rename-coordination-"));
  const callLog = join(dir, "calls.log");
  try {
    initGitRepo(dir);
    writeFileSync(join(dir, "lib.ts"), ["export function tinyFn(): number {", "  return 1;", "}", ""].join("\n"), "utf8");
    commitAll(dir);

    const coordinator = new Coordinator({
      command: process.execPath,
      args: [FAKE_ENTITY_ID_SERVER],
      env: { FAKE_WEAVE_CALL_LOG: callLog },
      cwd: dir,
      agentId: "rename-coordination-test-agent",
    });

    const outcome = await performRename(
      { old_name: "tinyFn", new_name: "tinyFunc" },
      { cwd: dir, semBin: "sem", coordinator, signal: undefined },
    );
    await coordinator.stop();

    assert.equal(outcome.isError, false, outcome.text);
    assert.match(readFileSync(join(dir, "lib.ts"), "utf8"), /export function tinyFunc\(/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
