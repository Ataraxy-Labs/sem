// LAW 4 (green half) -- ROLLBACK SAFETY: the compensating action restores
// the pre-edit bytes EXACTLY.
//
// STRUCTURE: performOneWeaveEdit's verify-extract failure path and its
// identity-guard refusal path are COMPENSATING TRANSACTIONS
// (Garcia-Molina & Salem 1987, "Sagas", SIGMOD): the forward action (the
// splice write) already committed to disk, so the failure path executes
// C(T) = write(currentContent). The law witnessed here is that C is an
// EXACT inverse in the single-writer frame: after rollback the file is
// byte-for-byte the pre-edit snapshot -- including the representation
// details an approximate inverse would normalize away (CRLF line endings,
// a missing trailing newline). Compare bytes, not parse trees.
//
// The RED half of this law -- the compensation write is UNGUARDED against
// a foreign writer (the torn-read clobber) -- is deterministically
// reproduced and preserved as an open defect in
// test/laws/rollback-unguarded-write.review.test.ts. Do not conflate the
// two: this file witnesses that C inverts T when nobody else wrote; the
// review file witnesses that C is applied blindly even when somebody did.
//
// Non-vacuity probes: each test first asserts the failure genuinely fired
// (isError + the specific refusal shape) AND that the forward write had
// really happened / would really have differed -- a rollback law over an
// edit that never landed anything witnesses nothing.
import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performWeaveEdit } from "../../src/tools/weave-edit.ts";

function makeDir(): string {
  return mkdtempSync(join(tmpdir(), "law-rollback-"));
}

test("verify-extract rollback: a replace that breaks the file's parse is rolled back to the exact pre-edit bytes", async () => {
  const dir = makeDir();
  try {
    const original = "export function target(): number {\n  return 1;\n}\n\nexport function other(): number {\n  return 2;\n}\n";
    writeFileSync(join(dir, "f.ts"), original);
    const outcome = await performWeaveEdit(
      // Unbalanced parens/braces: tree-sitter error recovery swallows the
      // NEIGHBOR entity, which is exactly what verifyEdit detects.
      { file: "f.ts", entity: { name: "target" }, op: "replace", content: "export function target(): number {\n  return ((1;\n" },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, true, "non-vacuity: verification must actually have failed");
    assert.match(outcome.text, /failed verification and was rolled back/);
    assert.equal(outcome.details.rolledBack, true);
    assert.equal(readFileSync(join(dir, "f.ts"), "utf8"), original, "byte-for-byte restoration");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("identity-guard rollback: a refused rename-by-replace restores CRLF + missing-trailing-newline bytes exactly", async () => {
  const dir = makeDir();
  try {
    // The exactness probe: CRLF throughout, no trailing newline -- any
    // rollback that re-renders lines instead of restoring the snapshot
    // would normalize one of these.
    const original = [
      "export function target(): number {",
      "  return 1;",
      "}",
      "export function other(): number {",
      "  return 2;",
      "}",
    ].join("\r\n");
    writeFileSync(join(dir, "f.ts"), original);
    const outcome = await performWeaveEdit(
      { file: "f.ts", entity: { name: "target" }, op: "replace", content: "export function renamed(): number {\n  return 1;\n}" },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, true, "non-vacuity: the identity guard must actually have refused");
    assert.match(outcome.text, /Refused and rolled back/);
    assert.equal(outcome.details.rolledBack, true);
    const restored = readFileSync(join(dir, "f.ts"), "utf8");
    assert.equal(restored, original, "byte-for-byte restoration, CRLF and trailing-newline-absence included");
    assert.ok(!restored.includes("renamed"), "no residue of the refused content");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("positive control: the same replace WITH allow_signature_change lands -- proving the guard (not the fixture) caused the rollback above", async () => {
  const dir = makeDir();
  try {
    const original = "export function target(): number {\n  return 1;\n}\n\nexport function other(): number {\n  return 2;\n}\n";
    writeFileSync(join(dir, "f.ts"), original);
    const outcome = await performWeaveEdit(
      { file: "f.ts", entity: { name: "target" }, op: "replace", content: "export function renamed(): number {\n  return 1;\n}", allow_signature_change: true },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);
    assert.ok(readFileSync(join(dir, "f.ts"), "utf8").includes("renamed"));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
