import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

const MAX_TEXT_LINES = 6;
// A single very long unwrapped line (e.g. every dependent's name inlined
// into one sentence) technically satisfies a line-count cap while still
// costing plenty of tokens — cap total length too, generously enough for a
// normal terse sentence but not for an inlined N-item list.
const MAX_TEXT_CHARS = 320;

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

// Eval data showed pi-sem costing far more tokens than vanilla pi per turn
// (ts-tokeff: 8k -> 102k); a fat weave_edit result text was a prime
// suspect. Detail belongs in `details` (already structured and complete),
// not repeated at length in the text field the model re-reads every turn.
test("an ambiguous name with many candidates keeps result text short, full list stays in details", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-terse-"));
  try {
    const classes = Array.from({ length: 12 }, (_, i) => `export class C${i} {\n  greet(): string {\n    return "hi ${i}";\n  }\n}`).join("\n\n");
    writeFileSync(join(dir, "many.ts"), `${classes}\n`, "utf8");

    const outcome = await run({ file: "many.ts", entity: { name: "greet" }, op: "delete" }, dir);
    assert.equal(outcome.isError, true);

    const lines = outcome.text.split("\n");
    assert.ok(lines.length <= MAX_TEXT_LINES, `expected <= ${MAX_TEXT_LINES} lines, got ${lines.length}:\n${outcome.text}`);

    const details = outcome.details as { candidates?: unknown[] };
    assert.equal(details.candidates?.length, 12, "the full candidate list must still be in details even though text is short");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a delete with many dependents keeps result text short, full list stays in details", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-terse-"));
  try {
    writeFileSync(join(dir, "math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n", "utf8");
    for (let i = 0; i < 12; i++) {
      writeFileSync(
        join(dir, `caller${i}.ts`),
        `import { add } from "./math.ts";\nexport function use${i}(): number {\n  return add(${i}, ${i});\n}\n`,
        "utf8",
      );
    }

    const outcome = await run({ file: "math.ts", entity: { name: "add" }, op: "delete" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const lines = outcome.text.split("\n");
    assert.ok(lines.length <= MAX_TEXT_LINES, `expected <= ${MAX_TEXT_LINES} lines, got ${lines.length}:\n${outcome.text}`);
    assert.ok(
      outcome.text.length <= MAX_TEXT_CHARS,
      `expected <= ${MAX_TEXT_CHARS} chars, got ${outcome.text.length} — the 12 dependent names must not all be inlined into the sentence:\n${outcome.text}`,
    );

    const details = outcome.details as { dependents?: { before?: unknown[] } };
    assert.equal(details.dependents?.before?.length, 12, "the full dependents list must still be in details even though text is short");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// A rename (allow_signature_change: true) leaves old callers referencing a
// name that no longer exists — the post-edit dependents check runs under the
// NEW name, so every pre-edit dependent shows up as "missing". That's the
// same fan-out shape as the delete case above, through formatDependentsText's
// separate `missing.length > 0` branch.
test("a rename with many now-dangling callers keeps result text short, full list stays in details", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-terse-"));
  try {
    writeFileSync(join(dir, "math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n", "utf8");
    for (let i = 0; i < 12; i++) {
      writeFileSync(
        join(dir, `caller${i}.ts`),
        `import { add } from "./math.ts";\nexport function use${i}(): number {\n  return add(${i}, ${i});\n}\n`,
        "utf8",
      );
    }

    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        content: "export function sum(a: number, b: number): number {\n  return a + b;\n}",
        allow_signature_change: true,
      },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);

    const lines = outcome.text.split("\n");
    assert.ok(lines.length <= MAX_TEXT_LINES, `expected <= ${MAX_TEXT_LINES} lines, got ${lines.length}:\n${outcome.text}`);
    assert.ok(
      outcome.text.length <= MAX_TEXT_CHARS,
      `expected <= ${MAX_TEXT_CHARS} chars, got ${outcome.text.length} — the 12 now-dangling caller names must not all be inlined into the sentence:\n${outcome.text}`,
    );

    const details = outcome.details as { dependents?: { before?: unknown[] } };
    assert.equal(details.dependents?.before?.length, 12, "the full before-dependents list must still be in details even though text is short");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
