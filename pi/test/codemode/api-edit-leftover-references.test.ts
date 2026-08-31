import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-edit-leftover-refs-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * Dogfood round 2, finding 6: code mode's task-3 rename
 * correctly renamed the entity's definition and updated the two callers'
 * call sites, but left the ORIGINAL import statements in place (unused,
 * dangling) and added a second, separate import for the new name instead
 * of editing the existing one -- both vanilla and tools-mode did a clean
 * rename on the same task. Root cause: sem.edit()'s response never told
 * the script whether it had just performed a RENAME at all (the new name
 * -- weave-edit.ts's own `afterEntityName` -- was silently discarded), let
 * alone which OTHER files still mention the OLD name and need a follow-up.
 * The model had no visibility into what it left behind.
 *
 * Fixed by having a rename (a "replace" whose identity check found a real
 * name change, allowed through via allow_signature_change) sweep the repo
 * for the OLD name (word-boundary-safe, so a prefix like resolveEntity
 * inside resolveEntityRef doesn't false-positive) and report every
 * remaining hit as `leftover_references`.
 */
test("a rename via sem.edit reports leftover_references for every file still mentioning the old name", async () => {
  await withTempCopy(["rename-def.ts", "rename-caller.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const result = (await api.edit({
      file: "rename-def.ts",
      entity: { name: "resolveEntity" },
      op: "replace",
      content: "export function resolveEntityRef(x: number): number {\n  return x;\n}",
      allow_signature_change: true,
    })) as { leftover_references: Array<{ file: string; line: number; snippet: string }> };

    assert.ok(Array.isArray(result.leftover_references), `expected a leftover_references array, got: ${JSON.stringify(result)}`);
    assert.ok(
      result.leftover_references.length >= 2,
      `expected at least 2 leftover references (the stale import + the still-old-named call site in rename-caller.ts), got: ${JSON.stringify(result.leftover_references)}`,
    );
    const files = new Set(result.leftover_references.map((r) => r.file));
    assert.ok(files.has("rename-caller.ts"), `expected rename-caller.ts to be flagged as a leftover reference, got files: ${[...files]}`);
    for (const ref of result.leftover_references) {
      assert.ok(typeof ref.line === "number" && ref.line > 0, `leftover reference missing a real line number: ${JSON.stringify(ref)}`);
      assert.ok(ref.snippet.length > 0, `leftover reference missing a snippet: ${JSON.stringify(ref)}`);
    }
  });
});

test("leftover_references is empty for an ordinary content edit that does NOT rename the entity", async () => {
  await withTempCopy(["rename-def.ts", "rename-caller.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.edit({
      file: "rename-def.ts",
      entity: { name: "resolveEntity" },
      op: "replace",
      content: "export function resolveEntity(x: number): number {\n  return x + 0;\n}",
    })) as { leftover_references: Array<{ file: string; line: number; snippet: string }> };

    assert.deepEqual(result.leftover_references, [], `a non-renaming edit must report no leftover references, got: ${JSON.stringify(result.leftover_references)}`);
  });
});

test("a batch sem.edit(request[]) rename also reports leftover_references per entity", async () => {
  await withTempCopy(["rename-def.ts", "rename-caller.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const results = (await api.edit([
      {
        file: "rename-def.ts",
        entity: { name: "resolveEntity" },
        op: "replace",
        content: "export function resolveEntityRef(x: number): number {\n  return x;\n}",
        allow_signature_change: true,
      },
    ])) as Array<{ leftover_references: Array<{ file: string; line: number; snippet: string }> }>;

    assert.equal(results.length, 1);
    assert.ok(results[0]!.leftover_references.length >= 2, `expected leftover references in the batch form too, got: ${JSON.stringify(results[0])}`);
  });
});
