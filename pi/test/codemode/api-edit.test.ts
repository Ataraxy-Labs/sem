import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
// Reuses the tool lane's fixtures read-only -- same math.ts/calculator.ts
// pair weave-edit-safety.test.ts already pins the underlying behavior
// against; this file proves api.ts's `edit` reaches the exact same guards
// through the codemode surface, not a reimplementation of them.
const FIXTURES = join(__dirname, "..", "tools", "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-api-edit-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("edit() refuses a replace that silently drops `export` and rolls the file back", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const original = readFileSync(join(dir, "math.ts"), "utf8");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    await assert.rejects(
      () =>
        api.edit({
          file: "math.ts",
          entity: { name: "add" },
          op: "replace",
          content: "function add(a: number, b: number): number {\n  return a + b;\n}",
        }),
      (err: Error) => {
        assert.match(err.message, /export/i);
        assert.match(err.message, /allow_signature_change/);
        return true;
      },
    );

    assert.equal(readFileSync(join(dir, "math.ts"), "utf8"), original, "file must be restored exactly");
  });
});

test("edit() rolls back a replace whose content breaks the file's syntax", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const original = readFileSync(join(dir, "math.ts"), "utf8");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    await assert.rejects(
      () =>
        api.edit({
          file: "math.ts",
          entity: { name: "add" },
          op: "replace",
          // Missing closing brace -- verify.ts's re-extraction check must
          // see a neighboring entity (`sub`) disappear and refuse this.
          content: "export function add(a: number, b: number): number {\n  return a + b;",
        }),
      (err: Error) => {
        assert.match(err.message, /rolled back|verif/i);
        return true;
      },
    );

    assert.equal(readFileSync(join(dir, "math.ts"), "utf8"), original, "file must be restored exactly");
  });
});

test("edit() with allow_signature_change: true lets an intentional visibility change through", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.edit({
      file: "math.ts",
      entity: { name: "add" },
      op: "replace",
      content: "function add(a: number, b: number): number {\n  return a + b;\n}",
      allow_signature_change: true,
    });
    assert.equal((result as { entity: { name: string } }).entity.name, "add");
    const finalContent = readFileSync(join(dir, "math.ts"), "utf8");
    assert.doesNotMatch(finalContent, /export function add/);
  });
});

test("edit(request[]) applies several edits across different files in one call", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const results = (await api.edit([
      {
        file: "math.ts",
        entity: { name: "sub" },
        op: "replace",
        content: "export function sub(a: number, b: number): number {\n  return a - b - 0;\n}",
      },
      {
        file: "calculator.ts",
        entity: { name: "calculate" },
        op: "replace",
        content: "export function calculate(a: number, b: number): number {\n  return add(a, b) + sub(a, b) + 0;\n}",
      },
    ])) as Array<{ entity: { name: string } }>;

    assert.equal(results.length, 2);
    assert.equal(results[0]!.entity.name, "sub");
    assert.equal(results[1]!.entity.name, "calculate");
    assert.match(readFileSync(join(dir, "math.ts"), "utf8"), /a - b - 0/);
    assert.match(readFileSync(join(dir, "calculator.ts"), "utf8"), /sub\(a, b\) \+ 0/);
  });
});

test("edit(request[]) reports one failing edit inline instead of aborting the rest of the batch", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const results = (await api.edit([
      { file: "math.ts", entity: { name: "doesNotExist" }, op: "replace", content: "export function doesNotExist() {}" },
      {
        file: "calculator.ts",
        entity: { name: "calculate" },
        op: "replace",
        content: "export function calculate(a: number, b: number): number {\n  return add(a, b) + sub(a, b) + 1;\n}",
      },
    ])) as Array<{ error?: string; entity?: { name: string } }>;

    assert.equal(results.length, 2);
    assert.ok(results[0]!.error, "the first request should report its failure inline, not throw");
    assert.match(results[0]!.error!, /no entity named "doesNotExist"/);
    assert.equal(results[1]!.entity?.name, "calculate", "the second, valid edit still lands despite the first one failing");
    assert.match(readFileSync(join(dir, "calculator.ts"), "utf8"), /sub\(a, b\) \+ 1/);
  });
});
