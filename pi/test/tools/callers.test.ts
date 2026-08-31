import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemCallers, type SemCallersParams } from "../../src/tools/sem-callers.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-callers-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemCallersParams, cwd: string) {
  return performSemCallers(params, { cwd, semBin: "sem" });
}

test("callers lists every call site of an unambiguous entity, one line per site", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run({ name: "add" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /calculator\.ts:3\s+in function calculate/);
  });
});

test("callers reports zero callers plainly instead of erroring", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ name: "sub" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /no callers/i);
  });
});

test("callers reports a not-found entity instead of throwing", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ name: "totallyMissingXYZ" }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named "totallyMissingXYZ"/);
  });
});

test("callers refuses with a candidate list when the name is ambiguous, instead of picking one", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-callers-test-"));
  try {
    cpSync(join(FIXTURES, "sample.ts"), join(dir, "sample.ts"));
    writeFileSync(
      join(dir, "usegreet.ts"),
      ['import { Alice } from "./sample.ts";', "export function useAlice() {", "  const a = new Alice();", "  return a.greet();", "}", ""].join(
        "\n",
      ),
      "utf8",
    );

    const outcome = await run({ name: "greet" }, dir);
    assert.equal(outcome.isError, true, "must refuse rather than silently pick one of the ambiguous entities");
    assert.match(outcome.text, /ambiguous/i);
    assert.match(outcome.text, /Alice/);
    assert.match(outcome.text, /Bob/);

    const details = outcome.details as { candidates?: Array<{ file?: string; type?: string; parent_name?: string | null; start_line?: number; end_line?: number }> };
    assert.equal(details.candidates?.length, 2);
    for (const c of details.candidates ?? []) {
      assert.ok(c.file, "each candidate must report its file");
      assert.ok(c.type, "each candidate must report its type");
      assert.ok(typeof c.start_line === "number" && typeof c.end_line === "number", "each candidate must report its line range");
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("limit defaults to 50 and caps with a trailing count", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-callers-test-"));
  try {
    writeFileSync(join(dir, "target.ts"), "export function target(): number {\n  return 1;\n}\n", "utf8");
    for (let i = 0; i < 60; i++) {
      writeFileSync(
        join(dir, `caller${i}.ts`),
        `import { target } from "./target.ts";\nexport function use${i}(): number {\n  return target();\n}\n`,
        "utf8",
      );
    }

    const outcome = await run({ name: "target" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { total?: number; shown?: number };
    assert.equal(details.shown, 50);
    assert.equal(details.total, 60);
    assert.match(outcome.text, /…10 more/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("limit= overrides the default cap", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-callers-test-"));
  try {
    writeFileSync(join(dir, "target.ts"), "export function target(): number {\n  return 1;\n}\n", "utf8");
    for (let i = 0; i < 10; i++) {
      writeFileSync(
        join(dir, `caller${i}.ts`),
        `import { target } from "./target.ts";\nexport function use${i}(): number {\n  return target();\n}\n`,
        "utf8",
      );
    }

    const outcome = await run({ name: "target", limit: 3 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { total?: number; shown?: number };
    assert.equal(details.shown, 3);
    assert.equal(details.total, 10);
    assert.match(outcome.text, /…7 more/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("entity_type narrows the query the same way sem_find's type= does", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run({ name: "add", entity_type: "function" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /calculator\.ts:3\s+in function calculate/);
  });
});
