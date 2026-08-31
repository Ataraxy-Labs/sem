import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemFind, type SemFindParams } from "../../src/tools/sem-find.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-find-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemFindParams, cwd: string) {
  return performSemFind(params, { cwd, semBin: "sem" });
}

test("find locates an entity by exact name with its file and line range", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ query: "add" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function add math\.ts:L1-L3/);
  });
});

test("find's exact name matching is case-sensitive and does not do substring matching (sem's own behavior)", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const wrongCase = await run({ query: "Add" }, dir);
    assert.equal(wrongCase.isError, false);
    assert.match(wrongCase.text, /no entit/i);

    const substring = await run({ query: "ad" }, dir);
    assert.equal(substring.isError, false);
    assert.match(substring.text, /no entit/i);
  });
});

test("find narrows to a specific kind with type=, excluding a wrong kind entirely", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const asMethod = await run({ query: "greet", type: "method" }, dir);
    assert.equal(asMethod.isError, false, asMethod.text);
    assert.match(asMethod.text, /method greet sample\.ts:L2-L4/);
    assert.match(asMethod.text, /method greet sample\.ts:L8-L10/);

    const wrongType = await run({ query: "greet", type: "class" }, dir);
    assert.equal(wrongType.isError, false);
    assert.match(wrongType.text, /no entit/i);
  });
});

test("find reports zero matches plainly instead of erroring", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ query: "totallyMissingName" }, dir);
    assert.equal(outcome.isError, false);
    assert.match(outcome.text, /no entit.*totallyMissingName/i);
  });
});

test("limit defaults to 20 when the caller doesn't pass one", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-find-test-"));
  try {
    for (let i = 0; i < 30; i++) {
      const sub = join(dir, `f${i}`);
      mkdirSync(sub, { recursive: true });
      writeFileSync(join(sub, "m.ts"), "export function dup(): number {\n  return 1;\n}\n", "utf8");
    }

    const outcome = await run({ query: "dup" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { total?: number; shown?: number };
    assert.equal(details.shown, 20);
    assert.equal(details.total, 30);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("queries= runs several lookups in one call instead of one call per query", async () => {
  await withTempCopy(["math.ts", "sample.ts"], async (dir) => {
    const outcome = await run({ queries: ["add", "standalone", "totallyMissingName"] }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    // Grouped by query, each still one-line-per-hit like the single form.
    assert.match(outcome.text, /"add"/);
    assert.match(outcome.text, /function add math\.ts:L1-L3/);
    assert.match(outcome.text, /"standalone"/);
    assert.match(outcome.text, /function standalone sample\.ts:L13-L15/);
    assert.match(outcome.text, /"totallyMissingName"/);
    assert.match(outcome.text, /no entit.*totallyMissingName/i);

    const details = outcome.details as { results?: Array<{ query?: string; total?: number; shown?: number }> };
    assert.equal(details.results?.length, 3);
    assert.equal(details.results?.[0]?.query, "add");
    assert.equal(details.results?.[0]?.total, 1);
    assert.equal(details.results?.[2]?.query, "totallyMissingName");
    assert.equal(details.results?.[2]?.total, 0);
  });
});

test("the single query= form is unaffected by batching — its details keep the hit-list shape, not a batch `results` wrapper", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const single = await run({ query: "add" }, dir);
    assert.equal(single.isError, false, single.text);
    const singleDetails = single.details as { results?: unknown; hits?: Array<{ name?: string }>; total?: number; shown?: number };
    assert.equal(singleDetails.results, undefined, "single query= form must not gain a batch `results` wrapper");
    assert.equal(singleDetails.hits?.[0]?.name, "add");
    assert.equal(singleDetails.total, 1);
  });
});

// queries= ran through Promise.all with no cap at
// all -- a caller passing an arbitrarily large array fired that many `sem
// find` child processes in one call. 30 queries against a 25-per-call cap
// must run exactly the first 25 and honestly report the other 5 as not run
// (never silently dropped, never silently run anyway).
test("queries= caps how many run per call, reporting the rest as not run instead of silently dropping or running them anyway", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const queries = Array.from({ length: 30 }, (_, i) => `missingQuery${i}`);
    const outcome = await run({ queries }, dir);

    assert.match(outcome.text, /…5 more not run/);
    const details = outcome.details as { total_queries?: number; ran?: number; omitted?: number; results?: unknown[] };
    assert.equal(details.total_queries, 30);
    assert.equal(details.ran, 25);
    assert.equal(details.omitted, 5);
    assert.equal(details.results?.length, 25);
  });
});

test("limit caps the number of results with a count of the rest", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-find-test-"));
  try {
    for (let i = 0; i < 60; i++) {
      const sub = join(dir, `f${i}`);
      mkdirSync(sub, { recursive: true });
      writeFileSync(join(sub, "m.ts"), "export function dup(): number {\n  return 1;\n}\n", "utf8");
    }

    const outcome = await run({ query: "dup", limit: 10 }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const details = outcome.details as { total?: number; shown?: number };
    assert.equal(details.shown, 10);
    assert.equal(details.total, 60);
    assert.match(outcome.text, /…50 more/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
