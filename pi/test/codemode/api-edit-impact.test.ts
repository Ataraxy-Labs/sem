import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

/**
 * FEATURE 1 (agent-first visibility): "the editing agent should see
 * consequence without asking a second question." Every successful
 * sem.edit()/sem.rename() receipt carries ONE compact `impact` line -- the
 * direct callers/referencers of the entity that was just edited, count plus
 * up to 5 names.
 *
 * These tests pin BOTH halves of the contract that makes it cheap: the line
 * is read back from the dependents weave_edit ALREADY captured before the
 * write (no second graph query, no shell, no type-check), and it is capped
 * by construction so it can never grow past a single line of the receipt.
 */

const __dirname = dirname(fileURLToPath(import.meta.url));
// Same math.ts/calculator.ts pair api-edit.test.ts uses: calculate() calls
// both add() and sub(), and nothing calls calculate() -- so the fixture
// gives a with-callers case and a no-callers case for free.
const FIXTURES = join(__dirname, "..", "tools", "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-api-edit-impact-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("edit() on an entity with callers reports them inline, named, in one impact line", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.edit({
      file: "math.ts",
      entity: { name: "add" },
      op: "replace",
      content: "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
    })) as { impact: string };

    assert.equal(result.impact, "1 caller (calculate)");
  });
});

test("edit() on an entity nothing calls says so plainly instead of staying silent", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.edit({
      file: "calculator.ts",
      entity: { name: "calculate" },
      op: "replace",
      content: "export function calculate(a: number, b: number): number {\n  return add(a, b) + sub(a, b);\n}\n",
    })) as { impact: string };

    assert.equal(result.impact, "no direct callers");
  });
});

test("the impact line names at most 5 callers and counts the rest -- one line, always", async () => {
  await withTempCopy([], async (dir) => {
    writeFileSync(join(dir, "core.ts"), "export function target(): number {\n  return 1;\n}\n");
    // Seven distinct referencing entities, each in its own file, so the
    // rendered line must cap the names and count the remainder.
    for (let i = 0; i < 7; i++) {
      writeFileSync(
        join(dir, `user${i}.ts`),
        `import { target } from "./core.ts";\n\nexport function caller${i}(): number {\n  return target();\n}\n`,
      );
    }
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.edit({
      file: "core.ts",
      entity: { name: "target" },
      op: "replace",
      content: "export function target(): number {\n  return 2;\n}\n",
    })) as { impact: string };

    assert.match(result.impact, /^7 callers \(/, `expected a 7-caller line, got: ${result.impact}`);
    assert.match(result.impact, /\+2 more\)$/, `expected the remainder counted, got: ${result.impact}`);
    assert.equal(result.impact.split(",").filter((s) => /caller\d/.test(s)).length, 5, "at most 5 names are spelled out");
    assert.ok(!result.impact.includes("\n"), "the impact line is never more than one line of the receipt");
  });
});

test("an insert edit, whose dependents were never captured, says not checked rather than claiming zero", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.edit({
      file: "math.ts",
      entity: { name: "add" },
      op: "insert_after",
      content: "export function mul(a: number, b: number): number {\n  return a * b;\n}",
    })) as { impact: string };

    assert.match(result.impact, /^not checked/, `expected an honest not-checked line, got: ${result.impact}`);
    assert.doesNotMatch(result.impact, /no direct callers/, "an unchecked edit must never be reported as zero callers");
  });
});

test("every entry of a batch edit carries its own impact line", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const results = (await api.edit([
      { file: "math.ts", entity: { name: "add" }, op: "replace", content: "export function add(a: number, b: number): number {\n  return a + b;\n}" },
      {
        file: "calculator.ts",
        entity: { name: "calculate" },
        op: "replace",
        content: "export function calculate(a: number, b: number): number {\n  return add(a, b) + sub(a, b);\n}",
      },
    ])) as Array<{ impact: string }>;

    assert.equal(results.length, 2);
    assert.equal(results[0]!.impact, "1 caller (calculate)");
    assert.equal(results[1]!.impact, "no direct callers");
  });
});

test("rename() carries the same impact line for the entity it renamed", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.rename("add", "plus")) as { applied: number; impact: string };

    assert.ok(result.applied > 0, "the rename itself must have applied");
    assert.equal(result.impact, "1 caller (calculate)");
  });
});
