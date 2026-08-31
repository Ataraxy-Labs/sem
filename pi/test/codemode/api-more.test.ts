import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, createHandleStore, createRunBudget } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-more-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * v2 item 5 (restraint), slice 3: more(handle) pages into a result a verb
 * already truncated for the token budget -- the omitted rows were already
 * computed and held in memory at truncation time, so paging is free (no
 * re-query). Reuses the SAME (already session-scoped) HandleStore every
 * other handle lives in.
 */

test("more(): a budget-truncated grep() result carries a more_handle that pages into the omitted rows", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    // "return" appears 3 times in calls.ts -- a tiny budget forces
    // truncation, leaving at least one row omitted.
    const budget = createRunBudget(5);
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const truncated = (await api.grep("return")) as { hits: unknown[]; total: number; more_handle?: string };
    assert.ok(typeof truncated.more_handle === "string", `expected a more_handle on a truncated result, got: ${JSON.stringify(truncated)}`);

    const page = (await api.more(truncated.more_handle!)) as { rows: unknown[]; remaining: number; more_handle?: string };
    assert.ok(page.rows.length > 0, "more() should return at least one previously-omitted row");
    assert.equal(page.rows.length + truncated.hits.length + page.remaining, truncated.total, "shown + paged + still-remaining should account for every real match");
  });
});

test("more(): an unknown/expired handle throws a clear error, not a silent misresolution", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    // more() is a synchronous verb (unlike the async sem.* calls), so it
    // throws directly rather than returning a rejected promise.
    assert.throws(() => api.more("h999"), /not a known pagination handle/i);
  });
});

test("more(): a handle that resolves to something OTHER than a pagination continuation (e.g. an entity locator handle) is refused, not silently misread", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const found = (await api.find("add")) as { hits: Array<{ h: string }> };
    assert.throws(() => api.more(found.hits[0]!.h), /not a known pagination handle/i);
  });
});

test("more(): a result with NOTHING omitted carries no more_handle at all", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const budget = createRunBudget();
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const result = (await api.find("add")) as { hits: unknown[]; more_handle?: string };
    assert.equal(result.more_handle, undefined);
  });
});

test("more(): pagination continuations are SESSION-scoped, same as any other handle -- a truncation from one buildSemApi() call pages via a later one sharing the SAME HandleStore", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const handles = createHandleStore();
    const budgetCall1 = createRunBudget(5);

    const apiCall1 = buildSemApi({ cwd: dir, semBin: "sem", handles, budget: budgetCall1 });
    const truncated = (await apiCall1.grep("return")) as { more_handle?: string };
    assert.ok(typeof truncated.more_handle === "string");

    const apiCall2 = buildSemApi({ cwd: dir, semBin: "sem", handles });
    const page = (await apiCall2.more(truncated.more_handle!)) as { rows: unknown[] };
    assert.ok(page.rows.length > 0, "a more_handle from an earlier sem_code call should page into a later one's more() call");
  });
});

test("more(): repeatedly paging eventually exhausts the remainder (more_handle becomes undefined)", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    // A very tight budget truncates aggressively, likely to 0-shown, 1 page.
    const budget = createRunBudget(1);
    const api = buildSemApi({ cwd: dir, semBin: "sem", budget });
    const truncated = (await api.grep("return")) as { more_handle?: string; total: number };
    assert.ok(typeof truncated.more_handle === "string");

    let handle: string | undefined = truncated.more_handle;
    let pages = 0;
    let totalPaged = 0;
    while (handle && pages < 10) {
      const page = (await api.more(handle)) as { rows: unknown[]; more_handle?: string };
      totalPaged += page.rows.length;
      handle = page.more_handle;
      pages += 1;
    }
    assert.ok(pages < 10, "pagination must terminate, not loop forever");
    assert.ok(totalPaged > 0);
  });
});
