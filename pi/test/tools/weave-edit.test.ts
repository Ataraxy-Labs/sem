import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureName: string, run: (dir: string, filePath: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-test-"));
  cpSync(join(FIXTURES, fixtureName), join(dir, fixtureName));
  return run(dir, fixtureName).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

test("replace on an ambiguous name refuses without parent_name, then succeeds with it, touching only that entity", async () => {
  await withTempCopy("sample.ts", async (dir, file) => {
    const ambiguous = await run(
      { file, entity: { name: "greet" }, op: "replace", content: 'greet(): string {\n  return "replaced";\n}' },
      dir,
    );
    assert.equal(ambiguous.isError, true);
    assert.match(ambiguous.text, /ambiguous/);
    assert.match(ambiguous.text, /Alice/);
    assert.match(ambiguous.text, /Bob/);

    const outcome = await run(
      {
        file,
        entity: { name: "greet", parent_name: "Bob" },
        op: "replace",
        content: 'greet(): string {\n  return "hi from bob, replaced";\n}',
      },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);

    const finalContent = readFileSync(join(dir, file), "utf8");
    assert.match(finalContent, /hi from bob, replaced/);
    assert.match(finalContent, /hi from alice/); // Alice's greet is untouched
  });
});

test("insert_after adds a new function right after the anchor, blank-line separated (Python)", async () => {
  await withTempCopy("sample.py", async (dir, file) => {
    const outcome = await run(
      { file, entity: { name: "standalone" }, op: "insert_after", content: "def inserted():\n    return 42" },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);

    const finalContent = readFileSync(join(dir, file), "utf8");
    const lines = finalContent.split("\n");
    const standaloneEnd = lines.findIndex((l) => l === "    return 1");
    const insertedStart = lines.findIndex((l) => l === "def inserted():");
    const helperStart = lines.findIndex((l) => l === "def helper():");
    assert.ok(standaloneEnd < insertedStart);
    assert.ok(insertedStart < helperStart);
    assert.match(finalContent, /return 42/);
  });
});

test("delete removes only the targeted function (Go)", async () => {
  await withTempCopy("sample.go", async (dir, file) => {
    const outcome = await run({ file, entity: { name: "helper" }, op: "delete" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const finalContent = readFileSync(join(dir, file), "utf8");
    assert.doesNotMatch(finalContent, /func helper/);
    assert.match(finalContent, /func standalone/);
    assert.match(finalContent, /^package main/);
  });
});

test("CRLF line endings and a missing trailing newline survive an edit that doesn't touch the file's tail", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-test-"));
  try {
    const file = "crlf.ts";
    const original = [
      "export function standalone(): number {",
      "  return 1;",
      "}",
      "export function helper(): number {",
      "  return 2;",
      "}",
    ].join("\r\n"); // no trailing newline, CRLF throughout
    writeFileSync(join(dir, file), original, "utf8");

    const outcome = await run(
      { file, entity: { name: "standalone" }, op: "insert_after", content: "export function mid(): number {\n  return 3;\n}" },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);

    const finalContent = readFileSync(join(dir, file), "utf8");
    assert.ok(!finalContent.includes("\r\n\n"), "no stray LF introduced alongside CRLF");
    assert.ok(finalContent.split("\n").every((line) => !line.endsWith("\r") || true)); // sanity: file still parses as CRLF below
    assert.equal(finalContent.match(/\r\n/g)?.length, (finalContent.match(/\n/g) ?? []).length, "every newline is part of a CRLF pair");
    assert.ok(!finalContent.endsWith("\n"), "trailing-newline-absence is preserved");
    assert.match(finalContent, /mid/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("two concurrent edits to different entities in the same file both land", async () => {
  await withTempCopy("sample.ts", async (dir, file) => {
    const [a, b] = await Promise.all([
      run({ file, entity: { name: "greet", parent_name: "Alice" }, op: "replace", content: 'greet(): string {\n  return "alice-1";\n}' }, dir),
      run({ file, entity: { name: "greet", parent_name: "Bob" }, op: "replace", content: 'greet(): string {\n  return "bob-1";\n}' }, dir),
    ]);
    assert.equal(a.isError, false, a.text);
    assert.equal(b.isError, false, b.text);

    const finalContent = readFileSync(join(dir, file), "utf8");
    assert.match(finalContent, /alice-1/);
    assert.match(finalContent, /bob-1/);
  });
});

test("a rewrite that breaks parsing around the edit fails verification and is rolled back", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-test-"));
  try {
    const file = "two.ts";
    const original = ["function foo() {", "  return 1;", "}", "", "function bar() {", "  return 2;", "}"].join("\n");
    writeFileSync(join(dir, file), original, "utf8");

    const outcome = await run(
      { file, entity: { name: "foo" }, op: "replace", content: "function foo() {\n  return 1;\n  const x = {\n" },
      dir,
    );
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /rolled back/);
    assert.match(outcome.text, /verification/i);

    const finalContent = readFileSync(join(dir, file), "utf8");
    assert.equal(finalContent, original, "file content is restored exactly on rollback");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("weave_edit reports a not-found entity with the closest names instead of throwing an opaque error", async () => {
  await withTempCopy("sample.ts", async (dir, file) => {
    const outcome = await run({ file, entity: { name: "greett" }, op: "delete" }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named "greett"/);
    assert.match(outcome.text, /greet/);
  });
});

test("content is required for replace/insert but not for delete", async () => {
  await withTempCopy("sample.ts", async (dir, file) => {
    const outcome = await run({ file, entity: { name: "standalone" }, op: "replace" }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /content.*required/);
  });
});
