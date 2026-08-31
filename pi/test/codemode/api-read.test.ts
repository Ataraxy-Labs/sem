import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-api-read-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("read(entity) with no `file` resolves a uniquely-named entity repo-wide", async () => {
  await withTempCopy(["unique.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.read({ name: "onlyOne" });
    const single = Array.isArray(result) ? result[0]! : result;
    assert.match(single.content, /return 42/);
    assert.equal(single.file, "unique.ts");
  });
});

test("read(entity) with no `file` throws with candidates when the name is ambiguous repo-wide", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.read({ name: "widget" }),
      (err: Error) => {
        assert.match(err.message, /ambiguous/i);
        assert.match(err.message, /dup-a\.ts/);
        assert.match(err.message, /dup-b\.ts/);
        return true;
      },
    );
  });
});

test("read(entity) with `file` set disambiguates the same repo-wide-ambiguous name", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.read({ name: "widget", file: "dup-a.ts" });
    const single = Array.isArray(result) ? result[0]! : result;
    assert.match(single.content, /widget from dup-a/);
  });
});

/**
 * v2 item 5 (restraint): reading MORE THAN ONE entity at once now defaults
 * to headers-only (signature+doc), not full bodies -- a script asking for
 * several entities is usually orienting, not about to paste them all into
 * an edit. `{ full: true }` opts back into the old full-bodies-array
 * behavior.
 */
test("read(entities[]) with 2+ entities defaults to headers-only, not full bodies", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts", "unique.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.read([
      { name: "widget", file: "dup-a.ts" },
      { name: "onlyOne" },
    ])) as { mode: string; note: string; entities: Array<{ name: string; signature: string; content?: string }> };
    assert.equal(result.mode, "headers");
    assert.match(result.note, /headers only/i);
    assert.match(result.note, /full: ?true/i, `note should tell the model literally how to get full bodies: ${result.note}`);
    assert.equal(result.entities.length, 2);
    assert.ok(result.entities.every((e) => typeof e.signature === "string" && e.signature.length > 0));
    assert.ok(result.entities.every((e) => e.content === undefined), "headers-only rows must not carry a full body");
  });
});

test("read(entities[], { full: true }) reads several FULL bodies in one call", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts", "unique.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const results = await api.read(
      [
        { name: "widget", file: "dup-a.ts" },
        { name: "onlyOne" },
      ],
      { full: true },
    );
    assert.ok(Array.isArray(results));
    assert.equal((results as unknown[]).length, 2);
    const rows = results as Array<{ content: string }>;
    assert.ok(rows.every((r) => typeof r.content === "string" && r.content.length > 0));
  });
});

test("read([entity]) with a SINGLE-element array still returns a full body -- only 2+ defaults to headers", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts", "unique.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const results = (await api.read([{ name: "widget", file: "dup-a.ts" }])) as Array<{ content: string }>;
    assert.ok(Array.isArray(results));
    assert.equal(results.length, 1);
    assert.match(results[0]!.content, /widget from dup-a/);
  });
});
