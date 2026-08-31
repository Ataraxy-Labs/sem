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
 * A finding (category: "does the identity
 * check now have false NEGATIVES after the pass-2 fix?" — explicitly asked
 * for in this round's brief).
 *
 * The pass-2 fix (`134e32b`, `src/tools/internal/identity.ts`'s
 * `hasNamedExport`) correctly closed the false-POSITIVE it was built for
 * (a legitimate named-export-list refactor no longer gets refused). But it
 * opened a false-NEGATIVE: `hasNamedExport` does a raw regex scan
 * (`/export\s*\{([^}]*)\}/g`) over the ENTIRE file's text with no
 * comment/string stripping. Any textual occurrence of `export { name }`
 * ANYWHERE in the file — including inside a `//` or `/* *\/` comment, a
 * JSDoc example, or a string literal — counts as proof the entity is
 * exported.
 *
 * Concrete, realistic trigger: a migration-note comment mentioning the OLD
 * export syntax right next to a function that has genuinely, deliberately
 * had its `export` keyword removed. `deriveVisibility` (called with the
 * real, comments-and-all `currentContent`/`spliced.text` at both call sites
 * in weave-edit.ts) sees the comment's text, concludes the entity is still
 * "exported," and `compareIdentity` reports NO visibility change — exactly
 * reopening the class of live bug (fb44a78, an `export` drop reaching disk
 * undetected because sem's own graph doesn't model visibility) this whole
 * feature exists to catch, whenever a nearby comment happens to mention
 * the export syntax.
 *
 * Untested: identity.test.ts's `hasNamedExport`-adjacent tests only cover
 * the true-export-list-in-real-code case; nothing exercises a
 * comment-only or string-literal-only mention.
 *
 * Fix sketch: strip comments/string literals before scanning for
 * `export { ... }` blocks (or, more robustly, delegate to a real parse —
 * sem itself already parses the file; if it could report the presence of
 * a named-export list for an entity, that would remove the need for a
 * hand-rolled regex heuristic entirely).
 */
test("a comment merely mentioning `export { name }` fools deriveVisibility into approving a genuine, undetected export drop", async () => {
  const dir = mkdtempSync(join(tmpdir(), "identity-comment-fools-"));
  try {
    // `helper` starts genuinely exported (inline `export`). The replacement
    // drops `export` for real -- no compensating named-export-list exists
    // anywhere in the file as actual code. The ONLY place the text
    // "export { helper }" appears is inside a comment, left over from an
    // unrelated migration note.
    writeFileSync(
      join(dir, "math.ts"),
      [
        "// Migration note: this module used to say `export { helper };` at the bottom; that's gone now, helper is internal-only.",
        "export function helper(a: number): number {",
        "  return a * 2;",
        "}",
        "",
      ].join("\n"),
      "utf8",
    );

    const original = readFileSync(join(dir, "math.ts"), "utf8");

    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "helper" },
        op: "replace",
        // Same body, `export` genuinely dropped -- no named-export-list
        // anywhere in the resulting file either.
        content: "function helper(a: number): number {\n  return a * 2;\n}",
      },
      dir,
    );

    assert.equal(
      outcome.isError,
      true,
      `weave_edit should have refused this genuine export-drop (the comment mentioning "export { helper }" is not real code and must not count) — ` +
        `got isError:false, meaning the edit landed. Result: ${outcome.text}`,
    );

    const finalContent = readFileSync(join(dir, "math.ts"), "utf8");
    assert.equal(finalContent, original, "the file must be rolled back to its original, still-exported state");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
