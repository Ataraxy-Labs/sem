import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, createRunBudget, DEFAULT_RUN_BUDGET_TOKENS } from "../../src/codemode/api.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-budget-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * v2 item 5 (restraint), slice 2: a PER-RUN token budget (default
 * DEFAULT_RUN_BUDGET_TOKENS) -- every sem.* result counts against it;
 * row-shaped results (find/grep/callers/blast/where) truncate with an
 * explicit `budget_note` once it's spent, rather than silently growing
 * unbounded. Per-run, not session-scoped -- only handles/dedup need
 * session scope.
 */

test("createRunBudget() defaults to DEFAULT_RUN_BUDGET_TOKENS", () => {
  const budget = createRunBudget();
  assert.equal(budget.total, DEFAULT_RUN_BUDGET_TOKENS);
  assert.equal(budget.used(), 0);
  assert.equal(budget.remaining(), DEFAULT_RUN_BUDGET_TOKENS);
});

test("createRunBudget(n) respects a custom total", () => {
  const budget = createRunBudget(100);
  assert.equal(budget.total, 100);
  budget.spend(30);
  assert.equal(budget.used(), 30);
  assert.equal(budget.remaining(), 70);
});

test("remaining() never goes negative even after overspending", () => {
  const budget = createRunBudget(10);
  budget.spend(50);
  assert.equal(budget.remaining(), 0);
});

test("a small result well within budget is returned untouched, no budget_note", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const budget = createRunBudget();
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const result = (await api.find("add")) as { hits: unknown[]; budget_note?: string };
    assert.ok(result.hits.length > 0);
    assert.equal(result.budget_note, undefined);
    assert.ok(budget.used() > 0, "the call must still count against the budget");
  });
});

test("grep()'s row-shaped result truncates with an explicit budget_note once a small budget is spent", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    // "return" appears 3 times in calls.ts -- a budget too small for all 3
    // hits' full JSON forces truncation.
    const budget = createRunBudget(5);
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const result = (await api.grep("return")) as { hits: unknown[]; total: number; budget_note?: string };
    assert.ok(result.total >= 3, `expected sem's own total to still report every real match, got ${result.total}`);
    assert.ok(result.hits.length < result.total, `expected the RETURNED hits to be truncated below total: hits=${result.hits.length} total=${result.total}`);
    assert.ok(typeof result.budget_note === "string" && result.budget_note.length > 0);
    assert.match(result.budget_note!, /budget/i);
  });
});

test("once the budget is exhausted, a call that returns almost nothing may be truncated to zero rows, not throw", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const budget = createRunBudget(1);
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const result = (await api.grep("return")) as { hits: unknown[]; budget_note?: string };
    assert.ok(Array.isArray(result.hits));
    assert.ok(typeof result.budget_note === "string");
  });
});

test("a non-row-shaped verb (outline) still counts against the budget even though it can't be truncated", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const budget = createRunBudget();
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    assert.equal(budget.used(), 0);
    await api.outline("calls.ts");
    assert.ok(budget.used() > 0, "outline()'s result must still be counted");
  });
});

test("budget is per-CALL to buildSemApi -- two separate buildSemApi() calls do NOT share spend by default", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const apiA = buildSemApi({ cwd: dir, semBin: "sem" });
    const apiB = buildSemApi({ cwd: dir, semBin: "sem" });
    await apiA.find("add");
    // No shared budget object was passed, so apiB has its own fresh
    // DEFAULT_RUN_BUDGET_TOKENS-sized budget -- nothing to assert on
    // apiB's internal state directly (it's not exposed), but this at
    // least proves apiB's call doesn't throw or behave as if spent.
    const result = (await apiB.find("add")) as { budget_note?: string };
    assert.equal(result.budget_note, undefined);
  });
});

test("e2e: sem_code's execute() result reports budget:{used,total} from a real sandboxed run", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const budget = createRunBudget(500);
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const result = await runInSandbox(`return await sem.find("add");`, { sem: api });
    assert.equal(result.ok, true, JSON.stringify(result));
    assert.equal(budget.total, 500);
    assert.ok(budget.used() > 0, "the real sandboxed find() call must have registered spend");
  });
});
