import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-safety-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

// Reproduces the real bug found in pi-sem's live e2e (pi-sem@4af1402): the
// model's replacement for `add` dropped the `export` keyword. weave_edit
// reported "Verification: ok" and calculator.ts's import silently broke —
// sem's own entity/impact graph is syntax-level and doesn't model
// TS export visibility, so nothing else would have caught this.
test("replace that silently drops `export` is refused and rolled back by default", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const original = readFileSync(join(dir, "math.ts"), "utf8");

    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        // Same signature and body, just missing `export` — the exact shape of the reported bug.
        content: "function add(a: number, b: number): number {\n  return a + b;\n}",
      },
      dir,
    );

    assert.equal(outcome.isError, true, outcome.text);
    assert.match(outcome.text, /export/i);
    assert.match(outcome.text, /allow_signature_change/);

    const finalContent = readFileSync(join(dir, "math.ts"), "utf8");
    assert.equal(finalContent, original, "file is restored exactly — the export-dropping edit must not land");
  });
});

test("replace that preserves export succeeds normally", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        content: "export function add(a: number, b: number): number {\n  return a + b + 0;\n}",
      },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    const finalContent = readFileSync(join(dir, "math.ts"), "utf8");
    assert.match(finalContent, /export function add/);
  });
});

test("allow_signature_change: true lets a visibility change through and reports affected dependents", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        content: "function add(a: number, b: number): number {\n  return a + b;\n}",
        allow_signature_change: true,
      },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    const finalContent = readFileSync(join(dir, "math.ts"), "utf8");
    assert.doesNotMatch(finalContent, /export function add/);
    // Dependents (calculator.ts's `calculate`) must be surfaced since the caller may now be broken.
    assert.match(outcome.text, /calculat/i);
  });
});

test("a delete with existing dependents reports them instead of pretending nothing depended on it", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run({ file: "math.ts", entity: { name: "add" }, op: "delete" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /calculat/i);
  });
});

test("replace that un-exports a Go function (capitalization drop) is refused and rolled back by default", async () => {
  await withTempCopy(["mathpkg_add.go", "mathpkg_use.go"], async (dir) => {
    const original = readFileSync(join(dir, "mathpkg_add.go"), "utf8");

    const outcome = await run(
      {
        file: "mathpkg_add.go",
        entity: { name: "Add" },
        op: "replace",
        content: "func add(a int, b int) int {\n\treturn a + b\n}",
      },
      dir,
    );

    assert.equal(outcome.isError, true, outcome.text);
    assert.match(outcome.text, /name changed/);

    const finalContent = readFileSync(join(dir, "mathpkg_add.go"), "utf8");
    assert.equal(finalContent, original);
  });
});

test("a delete with no dependents says so plainly", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    // Nothing in this fixture pair calls calculate() — it has no dependents.
    const outcome = await run({ file: "calculator.ts", entity: { name: "calculate" }, op: "delete" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /no other entities reference this/i);
  });
});
