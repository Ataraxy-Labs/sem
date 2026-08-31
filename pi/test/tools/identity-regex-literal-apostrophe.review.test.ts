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
 * PASS-4 FINDING (found independently; the brief explicitly asked to
 * re-verify whether identity.ts's pass-3 comment/string-stripper fix
 * introduced any new false NEGATIVES — confirmed yes).
 *
 * `stripCommentsAndStrings` (src/tools/internal/identity.ts) correctly
 * handles `//` line comments, `/* *\/` block comments, and quote-matched
 * `'...'`/`"..."`/`` `...` `` strings — but has ZERO awareness of regex
 * literals. A `/` is never treated specially unless immediately followed
 * by `/` or `*`; a genuine regex literal like `/don't match/` is copied
 * character-by-character like ordinary code. When that copy reaches the
 * apostrophe inside the regex, the stripper's own quote-matching logic
 * (`ch === "'"`) mistakes it for the START of a NEW single-quoted string
 * and begins scanning forward for the next unescaped `'` to close it. If
 * there is no OTHER apostrophe anywhere later in the file (a very ordinary
 * thing — most files don't have a second one specifically positioned to
 * "close" this accidental string), that scan runs all the way to EOF with
 * nothing to stop it — and everything from the stray apostrophe onward,
 * including any genuine `export { name };` statement further down the
 * file, gets blanked out along with it.
 *
 * Net effect: `hasNamedExport` can silently fail to see a real,
 * untouched, still-valid named-export-list statement whenever the file
 * contains ANY regex literal with an odd total count of unescaped
 * apostrophes anywhere before that statement — utterly unrelated to the
 * entity actually being edited. This reopens the exact pass-2 bug class
 * (a legitimate, visibility-preserving edit gets refused as a false
 * "visibility changed") the whole hasNamedExport mechanism exists to
 * prevent, just via a different trigger than pass-3's comment-mention one.
 *
 * Confirmed end-to-end: replacing `add`'s body to add nothing more than an
 * internal regex literal containing one contraction ("don't") makes
 * weave_edit refuse the edit and report "was exported, now not-exported"
 * — even though `add` is still, in truth, exported via the untouched
 * `export { add };` statement a few lines below, completely unaffected by
 * the edit.
 *
 * Untested: identity.test.ts's stripCommentsAndStrings-adjacent coverage
 * (via hasNamedExport/deriveVisibility) never exercises a regex literal at
 * all, let alone one containing an apostrophe.
 *
 * Fix sketch: at minimum, track paren/bracket/operator context well enough
 * to distinguish a `/` in regex-literal position (after `(`, `,`, `=`,
 * `return`, `{`, a binary operator, start-of-statement, ...) from a
 * division operator, and skip over a recognized regex literal the same
 * way string literals are skipped — using its own `/.../flags` closing
 * delimiter (respecting `[...]` character classes, where an unescaped `/`
 * does not close the regex) rather than leaving `/` completely
 * unhandled. A lower-effort mitigation: bound how far a single "string"
 * scan can run (e.g. stop at the next newline) so a false string-open
 * degrades to losing one line instead of the rest of the file — trades
 * one failure mode for a strictly smaller one.
 */
test("a regex literal with an unescaped apostrophe anywhere earlier in the file makes hasNamedExport blind to a real, untouched export statement further down", async () => {
  const dir = mkdtempSync(join(tmpdir(), "identity-regex-apostrophe-"));
  try {
    writeFileSync(
      dir + "/math.ts",
      ["function add(a: number, b: number): number {", "  return a + b;", "}", "", "export { add };", ""].join("\n"),
      "utf8",
    );

    const original = readFileSync(dir + "/math.ts", "utf8");

    const outcome = await run(
      {
        file: "math.ts",
        entity: { name: "add" },
        op: "replace",
        // The only change: add a regex literal with a single contraction
        // inside the body. Nothing about add's own exported-ness changes
        // at all -- it's still exported via the untouched `export { add }`
        // statement below, unaffected by this edit.
        content: "function add(a: number, b: number): number {\n  const re = /don't match/;\n  return a + b;\n}",
      },
      dir,
    );

    assert.equal(
      outcome.isError,
      false,
      `weave_edit should not refuse this edit -- "add" is still exported via the untouched "export { add };" statement, ` +
        `nothing about its visibility genuinely changed. A regex literal added inside its own body must not corrupt the ` +
        `export-list scan for the rest of the file. Got: ${outcome.text}`,
    );

    void original;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
