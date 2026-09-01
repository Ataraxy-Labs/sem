import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performSemGrep } from "../../src/tools/sem-grep.ts";
import { buildSemApi } from "../../src/codemode/api.ts";

/**
 * P7 of the 2026-09-02 transcript study -- the highest-frequency,
 * cheapest-to-fix friction in the whole corpus. `sem.grep` patterns were
 * always regex, and agents searching a codebase type CODE:
 *
 *   "is_fits("  "only("  "Prefetch("  "def none("  "related_objects("
 *   "col_suffixes=['"  "{str("  "F('"  "relim(.*Collection"
 *
 * 170 regex parse errors across 117 of 327 runs (36%), 25 of them fatal to
 * the whole script. A `literal: true` option escapes the pattern; the parse
 * error itself now names that option, so an agent that hits the wall once
 * is told the way over it.
 */

const SOURCE = [
  "def is_fits(path):",
  "    return path.endswith('.fits')",
  "",
  "def check(path):",
  "    col_suffixes=['a', 'b']",
  "    return is_fits(path) and col_suffixes",
  "",
  "def is_fits_like(path):",
  "    return True",
  "",
].join("\n");

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-grep-literal-"));
  writeFileSync(join(dir, "fits.py"), SOURCE, "utf8");
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("P7: an identifier with an open paren is a regex parse error WITHOUT literal, and matches WITH it", async () => {
  await withTempDir(async (dir) => {
    const asRegex = await performSemGrep({ pattern: "is_fits(" }, { cwd: dir, semBin: "sem" });
    assert.equal(asRegex.isError, true, "this is the 36%-of-runs failure the option exists for");
    assert.match(asRegex.text, /unclosed group|regex parse error/i);

    const asLiteral = await performSemGrep({ pattern: "is_fits(", literal: true }, { cwd: dir, semBin: "sem" });
    assert.equal(asLiteral.isError, false, asLiteral.text);
    const hits = asLiteral.details.hits as Array<{ line: number; text: string }>;
    assert.equal(hits.length, 2, "the definition and the call site -- is_fits_like( is a different string");
    assert.ok(hits.every((h) => h.text.includes("is_fits(")));
  });
});

test("P7: literal escapes brackets and quotes too, not just parens", async () => {
  await withTempDir(async (dir) => {
    const result = await performSemGrep({ pattern: "col_suffixes=['", literal: true }, { cwd: dir, semBin: "sem" });
    assert.equal(result.isError, false, result.text);
    assert.equal((result.details.hits as unknown[]).length, 1);
  });
});

test("P7: literal really is LITERAL -- a regex metacharacter matches only itself", async () => {
  await withTempDir(async (dir) => {
    // As a regex, `is_fits.` matches "is_fits(" and "is_fits_". As a
    // literal it matches neither, because no line contains "is_fits.".
    const asRegex = await performSemGrep({ pattern: "is_fits." }, { cwd: dir, semBin: "sem" });
    assert.equal(asRegex.isError, false);
    assert.ok((asRegex.details.hits as unknown[]).length > 0);

    const asLiteral = await performSemGrep({ pattern: "is_fits.", literal: true }, { cwd: dir, semBin: "sem" });
    assert.equal(asLiteral.isError, false);
    assert.equal((asLiteral.details.total as number) ?? 0, 0);
  });
});

test("P7: the details echo the pattern the caller TYPED, plus the literal flag -- never the escaped form", async () => {
  await withTempDir(async (dir) => {
    const result = await performSemGrep({ pattern: "is_fits(", literal: true }, { cwd: dir, semBin: "sem" });
    assert.equal(result.details.pattern, "is_fits(");
    assert.equal(result.details.literal, true);
  });
});

test("P7: a regex parse error NAMES literal:true, so one wall-hit teaches the way over it", async () => {
  await withTempDir(async (dir) => {
    const result = await performSemGrep({ pattern: "is_fits(" }, { cwd: dir, semBin: "sem" });
    assert.equal(result.isError, true);
    assert.match(result.text, /literal: ?true/);
  });
});

test("P7: literal applies to every pattern of a batch", async () => {
  await withTempDir(async (dir) => {
    const result = await performSemGrep({ patterns: ["is_fits(", "col_suffixes=['"], literal: true }, { cwd: dir, semBin: "sem" });
    assert.equal(result.isError, false, result.text);
    const results = result.details.results as Array<{ pattern: string; total: number }>;
    assert.deepEqual(
      results.map((r) => r.pattern),
      ["is_fits(", "col_suffixes=['"],
    );
    assert.ok(results.every((r) => r.total > 0));
  });
});

test("P7: sem.grep({literal:true}) is reachable from a code-mode script", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.grep("is_fits("), (err: Error) => {
      assert.match(err.message, /literal: ?true/);
      return true;
    });
    const result = (await api.grep("is_fits(", { literal: true })) as { total: number; literal: boolean };
    assert.equal(result.total, 2);
    assert.equal(result.literal, true);
  });
});
