import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, createRunBudget, createSessionBudget, DEFAULT_SESSION_BUDGET_CEILING } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-session-budget-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * v2 item 5/6: the gap team-lead's slice-2 review caught -- a PER-RUN
 * budget alone doesn't bind, since a script that exhausts 6k can just be
 * followed by another sem_code call with a fresh 6k, indefinitely.
 * SessionBudget tracks CUMULATIVE spend across every buildSemApi() call
 * sharing the same instance (mirroring separate sem_code invocations in
 * one pi session) and enforces a soft ceiling past which reads default to
 * headers-only even for a SINGLE entity and row results cap at 5,
 * regardless of what the per-run budget alone would still allow.
 */

test("createSessionBudget() defaults to DEFAULT_SESSION_BUDGET_CEILING, starts at zero use/runs", () => {
  const sessionBudget = createSessionBudget();
  assert.equal(sessionBudget.ceiling, DEFAULT_SESSION_BUDGET_CEILING);
  assert.equal(sessionBudget.used(), 0);
  assert.equal(sessionBudget.runs(), 0);
  assert.equal(sessionBudget.overCeiling(), false);
});

test("createSessionBudget(n) respects a custom ceiling", () => {
  const sessionBudget = createSessionBudget(100);
  assert.equal(sessionBudget.ceiling, 100);
  sessionBudget.spend(50);
  assert.equal(sessionBudget.overCeiling(), false);
  sessionBudget.spend(51);
  assert.equal(sessionBudget.overCeiling(), true);
});

test("sessionBudget accumulates spend across multiple calls sharing the same instance", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const sessionBudget = createSessionBudget();
    const api = buildSemApi({ cwd: dir, semBin: "sem", sessionBudget });
    assert.equal(sessionBudget.used(), 0);
    await api.find("add");
    const afterOne = sessionBudget.used();
    assert.ok(afterOne > 0);
    await api.find("twice");
    assert.ok(sessionBudget.used() > afterOne, "a second call must add to the SAME cumulative total, not reset it");
  });
});

test("read() of a SINGLE entity stays full-body under the ceiling, defaults to headers-only once over it", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const underCeiling = buildSemApi({ cwd: dir, semBin: "sem", sessionBudget: createSessionBudget() });
    const full = (await underCeiling.read({ name: "add", file: "calls.ts" })) as { content?: string; mode?: string };
    assert.ok(typeof full.content === "string" && full.content.length > 0, "under the ceiling, a single entity still gets its full body");

    const overCeilingBudget = createSessionBudget(1);
    overCeilingBudget.spend(2); // force overCeiling() true before any real call
    const overApi = buildSemApi({ cwd: dir, semBin: "sem", sessionBudget: overCeilingBudget });
    const headers = (await overApi.read({ name: "add", file: "calls.ts" })) as { mode?: string; entities?: unknown[]; note?: string };
    assert.equal(headers.mode, "headers", "once over the session ceiling, even a SINGLE entity read defaults to headers-only");
    assert.ok(Array.isArray(headers.entities) && headers.entities.length === 1);
    assert.match(headers.note ?? "", /session budget high/i);
  });
});

test("read() with { full: true } still returns a full body even when over the session ceiling", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const overCeilingBudget = createSessionBudget(1);
    overCeilingBudget.spend(2);
    const api = buildSemApi({ cwd: dir, semBin: "sem", sessionBudget: overCeilingBudget });
    const full = (await api.read({ name: "add", file: "calls.ts" }, { full: true })) as { content?: string };
    assert.ok(typeof full.content === "string" && full.content.length > 0, "{ full: true } must still override the session ceiling's default");
  });
});

// chain.ts (4 functions) + loop.ts (3) + calls.ts (3) + unique.ts (1) = 11
// "function" matches -- well over the session-ceiling row cap (5), needed
// to actually observe capping (a single small fixture doesn't have enough
// real hits to demonstrate the cap kicking in below the per-run budget).
const MANY_FUNCTION_FIXTURES = ["chain.ts", "loop.ts", "calls.ts", "unique.ts"];

test("row-shaped results cap at 5 once over the session ceiling, even when the per-run budget alone would allow more", async () => {
  await withTempCopy(MANY_FUNCTION_FIXTURES, async (dir) => {
    const overCeilingBudget = createSessionBudget(1);
    overCeilingBudget.spend(2);
    // A generous per-run budget -- proves the CAP comes from the session
    // ceiling, not from the per-run budget running out.
    const runBudget = createRunBudget(1_000_000);
    const api = buildSemApi({ cwd: dir, semBin: "sem", sessionBudget: overCeilingBudget, budget: runBudget });
    const result = (await api.grep("function")) as { hits: unknown[]; total: number; budget_note?: string };
    assert.ok(result.total > 5, `expected more than 5 real matches to make this a meaningful test, got ${result.total}`);
    assert.ok(result.hits.length <= 5, `expected rows capped at 5 once over the session ceiling, got ${result.hits.length}`);
    assert.match(result.budget_note ?? "", /session budget high/i);
  });
});

test("row-shaped results stay uncapped-by-session under the ceiling (per-run budget still applies normally)", async () => {
  await withTempCopy(MANY_FUNCTION_FIXTURES, async (dir) => {
    const sessionBudget = createSessionBudget();
    const api = buildSemApi({ cwd: dir, semBin: "sem", sessionBudget, budget: createRunBudget(1_000_000) });
    const result = (await api.grep("function")) as { hits: unknown[]; total: number; budget_note?: string };
    assert.equal(result.hits.length, result.total, "under the session ceiling, with a generous per-run budget, nothing should be capped");
    assert.equal(result.budget_note, undefined);
  });
});

test("sessionBudget is isolated by DEFAULT across separate buildSemApi() constructions that don't share an instance", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const apiA = buildSemApi({ cwd: dir, semBin: "sem" });
    await apiA.find("add");
    // apiB has its OWN fresh SessionBudget -- can't observe it directly,
    // but a fresh session should never be over any reasonable ceiling.
    const apiB = buildSemApi({ cwd: dir, semBin: "sem" });
    const full = (await apiB.read({ name: "add", file: "calls.ts" })) as { content?: string; mode?: string };
    assert.notEqual(full.mode, "headers", "a fresh, unshared SessionBudget should not already be over ceiling");
  });
});
