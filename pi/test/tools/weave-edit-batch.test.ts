import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

// Dogfood round 1, finding 4: a 9-site rename forced the
// model to choose between 9 slow single-entity weave_edit calls or one
// unaudited apply_patch bypass sweeping every remaining reference — it chose
// the bypass for 8 of the 9 sites. edits=[...] lets N entities, across one
// or many files, go through weave_edit's own verified path in one call.
test("edits= applies several entities across multiple files in one call, each with its own result line", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-"));
  try {
    writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");
    writeFileSync(join(dir, "b.ts"), "export function bar(): number {\n  return 2;\n}\n", "utf8");

    const outcome = await run(
      {
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "bar" }, op: "replace", content: "export function bar(): number {\n  return 20;\n}" },
        ],
      },
      dir,
    );

    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /2\/2 edits applied/);
    assert.match(outcome.text, /a\.ts#foo/);
    assert.match(outcome.text, /b\.ts#bar/);

    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), "export function foo(): number {\n  return 10;\n}\n");
    assert.equal(readFileSync(join(dir, "b.ts"), "utf8"), "export function bar(): number {\n  return 20;\n}\n");

    const details = outcome.details as { total?: number; succeeded?: number; results?: Array<{ file?: string; isError?: boolean }> };
    assert.equal(details.total, 2);
    assert.equal(details.succeeded, 2);
    assert.equal(details.results?.length, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("edits= (non-atomic, default): one failing edit doesn't stop or undo the others", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-"));
  try {
    writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");
    writeFileSync(join(dir, "b.ts"), "export function bar(): number {\n  return 2;\n}\n", "utf8");

    const outcome = await run(
      {
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "totallyMissingName" }, op: "replace", content: "whatever" },
          { file: "b.ts", entity: { name: "bar" }, op: "replace", content: "export function bar(): number {\n  return 20;\n}" },
        ],
      },
      dir,
    );

    assert.equal(outcome.isError, false, `two of three succeeding must not fail the whole batch. Got: ${outcome.text}`);
    assert.match(outcome.text, /2\/3 edits applied/);
    assert.match(outcome.text, /\[FAILED\]/);

    // Both a.ts's and b.ts's SUCCESSFUL edits must have actually landed —
    // the failure on the middle entity must not have rolled anything back.
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), "export function foo(): number {\n  return 10;\n}\n");
    assert.equal(readFileSync(join(dir, "b.ts"), "utf8"), "export function bar(): number {\n  return 20;\n}\n");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("edits= atomic=true rolls back every edit in the batch when one fails, even across different files", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-"));
  try {
    const originalA = "export function foo(): number {\n  return 1;\n}\n";
    const originalB = "export function bar(): number {\n  return 2;\n}\n";
    writeFileSync(join(dir, "a.ts"), originalA, "utf8");
    writeFileSync(join(dir, "b.ts"), originalB, "utf8");

    const outcome = await run(
      {
        atomic: true,
        edits: [
          { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 10;\n}" },
          { file: "b.ts", entity: { name: "totallyMissingName" }, op: "replace", content: "whatever" },
        ],
      },
      dir,
    );

    assert.equal(outcome.isError, true, "an atomic batch that had to roll back must report failure");
    assert.match(outcome.text, /rolled back/i);

    // a.ts's edit DID succeed before b.ts's failure was hit — atomic must
    // undo it, restoring the file to its exact pre-batch content.
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), originalA, "a.ts must be rolled back to its pre-batch content");
    assert.equal(readFileSync(join(dir, "b.ts"), "utf8"), originalB, "b.ts must be unchanged (its own edit never succeeded)");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the single file=/entity=/op= form is unaffected by batching — same text/details shape as before", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-batch-"));
  try {
    writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");

    const outcome = await run(
      { file: "a.ts", entity: { name: "foo" }, op: "replace", content: "export function foo(): number {\n  return 2;\n}" },
      dir,
    );

    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /^weave_edit: replace foo/);
    assert.doesNotMatch(outcome.text, /\(batch\)/);

    const details = outcome.details as { results?: unknown; total?: unknown };
    assert.equal(details.results, undefined, "single form must not gain a batch `results` wrapper");
    assert.equal(details.total, undefined, "single form must not gain a batch `total` field");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
