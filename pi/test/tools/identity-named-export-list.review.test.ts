import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

/**
 * A category c finding: "identity-check false positives that would block
 * a legitimate edit — look for a change that ISN'T really an
 * identity/visibility change but the heuristic in identity.ts flags as
 * one anyway".
 *
 * src/tools/internal/identity.ts's deriveVisibility() for TS/JS (line ~46):
 *
 *   return /^\s*export\s+(default\s+)?/.test(sourceLine) ? "exported" : "not-exported";
 *
 * This only looks at the entity's OWN declaration line. TypeScript/JS also
 * supports exporting a function via a separate named-export-list statement
 * elsewhere in the file (`export { add };`) without an inline `export`
 * keyword on the function itself — the function is fully, genuinely
 * exported either way; it's a stylistic choice, not a visibility change.
 * The module's own doc comment even lists "TS named-export lists
 * `export { x }`" as an unhandled mechanism and claims deriveVisibility's
 * fallback for anything it can't model is `"unknown"` ("'unknown' is the
 * honest answer for anything outside that"). That claim is not accurate
 * for TS/JS: the switch in deriveVisibility always returns a definite
 * "exported"/"not-exported" for .ts/.js files — "unknown" is only reached
 * for `languageOf() === "other"` (an unrecognized extension). So a
 * refactor from `export function add(...)  { ... }` to `function add(...)
 * { ... }` PLUS a trailing `export { add };` — still fully exported,
 * nothing broken — gets a confident, WRONG "not-exported" verdict, and
 * weave_edit refuses + rolls back a harmless, correct edit unless the
 * caller passes allow_signature_change:true (which they have no reason to
 * think they need, since nothing about this entity's visibility actually
 * changed).
 *
 * Untested: test/tools/weave-edit-safety.test.ts's identity-check tests
 * only cover genuine drops (no export anywhere) and Go's capitalization
 * rule; nothing exercises the named-export-list style.
 */
test("a refactor from inline `export function` to a bare function + a separate `export { name }` statement is a false-positive identity-change refusal", async () => {
  const dir = mkdtempSync(join(tmpdir(), "identity-named-export-"));
  try {
    // math.ts: `add` is exported BOTH ways at once — inline `export` on its
    // own declaration AND a separate named-export-list statement at the
    // bottom (redundant on purpose: this way the edit below only ever
    // touches the function entity itself, and the trailing `export { add }`
    // statement is untouched proof that the entity remains exported
    // throughout, independent of the heuristic's blind spot).
    writeFileSync(
      join(dir, "math.ts"),
      ["export function add(a: number, b: number): number {", "  return a + b;", "}", "", "export { add };", ""].join("\n"),
      "utf8",
    );
    writeFileSync(
      join(dir, "calculator.ts"),
      ['import { add } from "./math.ts";', "", "export function calculate(a: number, b: number): number {", "  return add(a, b);", "}", ""].join(
        "\n",
      ),
      "utf8",
    );

    // The replacement drops only the inline `export` keyword on the
    // function's own declaration — the untouched `export { add };`
    // statement at the bottom of the file still exports it. `add` remains,
    // in truth, fully exported throughout. This is a legitimate stylistic
    // refactor, not a visibility change.
    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        content: "function add(a: number, b: number): number {\n  return a + b;\n}",
      },
      dir,
    );

    assert.equal(
      outcome.isError,
      false,
      `weave_edit should not refuse this refactor — "add" is still exported via a named-export-list elsewhere in the file, ` +
        `so nothing about its visibility genuinely changed. Got: ${outcome.text}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
