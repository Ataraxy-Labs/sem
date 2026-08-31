import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

// Dogfood round 1, finding 2: a same-signature,
// doc-comment-only replace on a Rust function was refused as a false
// "identity changed ... to (no entity found at the edited location)". Root
// cause: sem's own start_line for the post-edit entity moves PAST a leading
// /// doc comment (confirmed empirically: a bare `pub fn tokenize` has
// start_line 1; add a "/// ..." line above it and sem reports start_line 2
// for the SAME function) — but weave-edit.ts looked the after-entity up by
// `e.start_line === spliced.newRange.start`, which still points at the
// doc-comment's own line (the first line of the new content), so the exact
// match found nothing at all. The real e2e case: the model's response to
// this refusal was to bypass weave_edit entirely via apply_patch, with zero
// verification/coordination — the highest-priority finding of the round.
test("Rust: adding a leading /// doc comment on replace is not a false identity change", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-dogfood-"));
  try {
    writeFileSync(
      join(dir, "expr.rs"),
      ["pub fn tokenize(src: &str) -> Option<Vec<String>> {", "    Some(vec![src.to_string()])", "}", ""].join("\n"),
      "utf8",
    );

    const outcome = await run(
      {
        file: "expr.rs",
        entity: { name: "tokenize" },
        op: "replace",
        content: [
          "/// Splits source text into tokens.",
          "pub fn tokenize(src: &str) -> Option<Vec<String>> {",
          "    Some(vec![src.to_string()])",
          "}",
        ].join("\n"),
      },
      dir,
    );

    assert.equal(
      outcome.isError,
      false,
      `a doc-comment-only addition must not be reported as an identity change. Got: ${outcome.text}`,
    );
    assert.doesNotMatch(outcome.text, /no entity found at the edited location/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Same defect class, TS JSDoc — sem's start_line for a function also moves
// past a leading /** ... */ block (confirmed empirically the same way).
test("TS: adding a leading JSDoc block on replace is not a false identity change", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-dogfood-"));
  try {
    writeFileSync(join(dir, "math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n", "utf8");

    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        content: ["/**", " * Adds two numbers.", " */", "export function add(a: number, b: number): number {", "  return a + b;", "}"].join("\n"),
      },
      dir,
    );

    assert.equal(outcome.isError, false, `a JSDoc-only addition must not be reported as an identity change. Got: ${outcome.text}`);
    assert.doesNotMatch(outcome.text, /no entity found at the edited location/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Python's case is actually different (confirmed empirically): sem keeps a
// decorator INSIDE the entity's own start_line, so this case never hit the
// positional-lookup bug in the first place — recorded here as a regression
// guard now that the lookup logic changes, not a repro of a new bug.
test("Python: adding a leading decorator on replace is not a false identity change", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-dogfood-"));
  try {
    writeFileSync(join(dir, "deco.py"), "def plain():\n    return 1\n", "utf8");

    const outcome = await run(
      {
        file: "deco.py",
        entity: { name: "plain" },
        op: "replace",
        content: ["@staticmethod", "def plain():", "    return 1"].join("\n"),
        allow_signature_change: true, // Python has no `export` concept sem models; this only guards the identity-lookup itself.
      },
      dir,
    );

    assert.equal(outcome.isError, false, `a decorator-only addition must not be reported as an identity change. Got: ${outcome.text}`);
    assert.doesNotMatch(outcome.text, /no entity found at the edited location/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The lookup fix must not regress the case it exists to guard against: a
// GENUINE rename hidden behind a doc-comment-shifted range must still be
// caught and reported accurately (not silently waved through as "unchanged"
// just because something is found somewhere in the range).
test("a genuine rename behind a leading doc comment is still caught as an identity change", async () => {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-dogfood-"));
  try {
    writeFileSync(
      join(dir, "expr.rs"),
      ["pub fn tokenize(src: &str) -> Option<Vec<String>> {", "    Some(vec![src.to_string()])", "}", ""].join("\n"),
      "utf8",
    );

    const outcome = await run(
      {
        file: "expr.rs",
        entity: { name: "tokenize" },
        op: "replace",
        content: [
          "/// Splits source text into tokens.",
          "pub fn lex(src: &str) -> Option<Vec<String>> {",
          "    Some(vec![src.to_string()])",
          "}",
        ].join("\n"),
      },
      dir,
    );

    assert.equal(outcome.isError, true, "a real rename must still be refused without allow_signature_change");
    assert.match(outcome.text, /name changed from "tokenize" to "lex"/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
