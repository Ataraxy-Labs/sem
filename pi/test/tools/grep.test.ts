import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemGrep, type SemGrepParams } from "../../src/tools/sem-grep.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-grep-test-"));
  for (const name of fixtureNames) {
    const dest = join(dir, name);
    mkdirSync(dirname(dest), { recursive: true });
    cpSync(join(FIXTURES, name), dest);
  }
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemGrepParams, cwd: string) {
  return performSemGrep(params, { cwd, semBin: "sem" });
}

test("grep finds literal/regex text hits with file:line:text", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ pattern: "export function" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /sample\.ts:L13: export function standalone/);
    assert.match(outcome.text, /sample\.ts:L17: export function helper/);
  });
});

test("grep reports zero matches plainly instead of erroring", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ pattern: "totallyAbsentPattern123" }, dir);
    assert.equal(outcome.isError, false);
    assert.match(outcome.text, /no match/i);
  });
});

test("path= narrows hits to a single file or directory prefix", async () => {
  await withTempCopy(["sample.ts", "subdir/nested.ts"], async (dir) => {
    const all = await run({ pattern: "export function" }, dir);
    assert.match(all.text, /sample\.ts/);
    assert.match(all.text, /nested\.ts/);

    const scoped = await run({ pattern: "export function", path: "subdir" }, dir);
    assert.equal(scoped.isError, false, scoped.text);
    assert.doesNotMatch(scoped.text, /sample\.ts/);
    assert.match(scoped.text, /subdir\/nested\.ts/);
  });
});

test("glob= narrows hits by filename pattern", async () => {
  await withTempCopy(["sample.ts", "subdir/nested.ts"], async (dir) => {
    const outcome = await run({ pattern: "export function", glob: "subdir/**" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.doesNotMatch(outcome.text, /(?<!\/)sample\.ts/);
    assert.match(outcome.text, /subdir\/nested\.ts/);
  });
});

test("context= includes surrounding lines around each hit", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ pattern: "return 1;", context: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    // The hit is inside standalone(); with context=1 the surrounding
    // signature/closing-brace lines should also show up somewhere.
    assert.match(outcome.text, /export function standalone/);
    assert.match(outcome.text, /return 1;/);
  });
});

test("patterns= runs several searches in one call instead of one call per pattern", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ patterns: ["export function", "totallyAbsentPattern123", "return 1;"] }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    // Grouped by pattern, each still one-line-per-hit like the single form.
    assert.match(outcome.text, /"export function"/);
    assert.match(outcome.text, /sample\.ts:L13: export function standalone/);
    assert.match(outcome.text, /sample\.ts:L17: export function helper/);
    assert.match(outcome.text, /"totallyAbsentPattern123"/);
    assert.match(outcome.text, /no match/i);
    assert.match(outcome.text, /"return 1;"/);
    assert.match(outcome.text, /sample\.ts:L14:.*return 1;/);

    const details = outcome.details as { results?: Array<{ pattern?: string; total?: number; shown?: number }> };
    assert.equal(details.results?.length, 3);
    assert.equal(details.results?.[0]?.pattern, "export function");
    assert.equal(details.results?.[0]?.total, 2);
    assert.equal(details.results?.[1]?.pattern, "totallyAbsentPattern123");
    assert.equal(details.results?.[1]?.total, 0);
  });
});

test("patterns= applies the same path=/glob=/limit= to every pattern in the batch", async () => {
  await withTempCopy(["sample.ts", "subdir/nested.ts"], async (dir) => {
    const outcome = await run({ patterns: ["export function", "export const"], path: "subdir" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.doesNotMatch(outcome.text, /(?<!\/)sample\.ts/);
  });
});

// patterns= ran through Promise.all with no cap
// at all -- a caller passing an arbitrarily large array fired that many
// `sem grep` child processes in one call. 30 patterns against a
// 25-per-call cap must run exactly the first 25 and honestly report the
// other 5 as not run (never silently dropped, never silently run anyway).
test("patterns= caps how many run per call, reporting the rest as not run instead of silently dropping or running them anyway", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const patterns = Array.from({ length: 30 }, (_, i) => `totallyMissingPattern${i}`);
    const outcome = await run({ patterns }, dir);

    assert.match(outcome.text, /…5 more not run/);
    const details = outcome.details as { total_patterns?: number; ran?: number; omitted?: number; results?: unknown[] };
    assert.equal(details.total_patterns, 30);
    assert.equal(details.ran, 25);
    assert.equal(details.omitted, 5);
    assert.equal(details.results?.length, 25);
  });
});

test("the single pattern= form is unaffected by batching — no `results` wrapper leaks into its details", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const single = await run({ pattern: "export function" }, dir);
    assert.equal(single.isError, false, single.text);
    const singleDetails = single.details as { results?: unknown; total?: number };
    assert.equal(singleDetails.results, undefined, "single pattern= form must not gain a batch `results` wrapper");
    assert.equal(singleDetails.total, 2);
  });
});

test("limit defaults to 20 when the caller doesn't pass one", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-grep-test-"));
  try {
    const lines = Array.from({ length: 30 }, (_, i) => `const marker${i} = "needle";`);
    writeFileSync(join(dir, "many.ts"), lines.join("\n") + "\n", "utf8");

    const outcome = await run({ pattern: "needle" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { total?: number; shown?: number };
    assert.equal(details.shown, 20);
    assert.equal(details.total, 30);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("limit caps the number of hits with a count of the rest", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-grep-test-"));
  try {
    const lines = Array.from({ length: 60 }, (_, i) => `const marker${i} = "needle";`);
    writeFileSync(join(dir, "many.ts"), lines.join("\n") + "\n", "utf8");

    const outcome = await run({ pattern: "needle", limit: 10 }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const details = outcome.details as { total?: number; shown?: number };
    assert.equal(details.shown, 10);
    assert.equal(details.total, 60);
    assert.match(outcome.text, /…50 more/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
