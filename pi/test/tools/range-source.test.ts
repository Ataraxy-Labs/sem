import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditParams } from "../../src/tools/weave-edit.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "weave-edit-range-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: WeaveEditParams, cwd: string) {
  return performWeaveEdit(params, { cwd, semBin: "sem", coordinator: undefined });
}

// sem 0.23.0 gives correct start_line/end_line for non-code entities
// (markdown headings, toml/yaml sections, ...) but no reliable byte range
// (start_byte/end_byte come back 0/0, or omitted entirely — confirmed
// empirically for markdown with this install: the fields are absent).
// weave_edit must never splice using byte offsets it can't trust — it has
// to derive the splice purely from start_line/end_line here, and say so.
test("replace on a markdown heading section works with no reliable byte range from sem", async () => {
  await withTempCopy(["notes.md"], async (dir) => {
    const outcome = await run(
      {
        file: "notes.md",
        entity: { name: "Section One" },
        op: "replace",
        content: "## Section One\n\nReplaced content for section one.",
      },
      dir,
    );

    assert.equal(outcome.isError, false, outcome.text);

    const finalContent = readFileSync(join(dir, "notes.md"), "utf8");
    assert.match(finalContent, /Replaced content for section one\./);
    assert.match(finalContent, /## Section Two/);
    assert.match(finalContent, /Content for section two\./);
    // Section One's own old body must be gone, not just appended alongside.
    assert.doesNotMatch(finalContent, /^Content for section one\.$/m);
  });
});

test("a successful edit reports which range source it used to splice", async () => {
  await withTempCopy(["notes.md"], async (dir) => {
    const outcome = await run(
      { file: "notes.md", entity: { name: "Section Two" }, op: "replace", content: "## Section Two\n\nNew content." },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { entity?: { range_source?: string } };
    assert.equal(details.entity?.range_source, "line");
  });
});

// Whether sem returns a byte range for a markdown entity at all is
// sem-version-dependent (confirmed empirically: sem 0.23.0 omits it,
// 0.23.1+ includes one) — this live smoke only checks that weave_edit
// threads whatever internal/entities.ts's deriveEntity computed through to
// its own output as a real boolean, without pinning which value that is
// for this specific sem version. The actual true/false-per-shape guarantee
// is covered precisely by entities.test.ts's deriveEntity tests, which
// feed both captured shapes through the same code directly.
test("range source reporting includes byte_range_reliable as a real boolean for a markdown entity", async () => {
  await withTempCopy(["notes.md"], async (dir) => {
    const outcome = await run(
      { file: "notes.md", entity: { name: "Section Two" }, op: "replace", content: "## Section Two\n\nNew content." },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { entity?: { byte_range_reliable?: boolean } };
    assert.equal(typeof details.entity?.byte_range_reliable, "boolean");
  });
});

test("range source reporting says a code entity's byte range was reliable (regression check)", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run(
      { file: "sample.ts", entity: { name: "standalone" }, op: "replace", content: "export function standalone(): number {\n  return 99;\n}" },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { entity?: { range_source?: string; byte_range_reliable?: boolean } };
    assert.equal(details.entity?.range_source, "line");
    assert.equal(details.entity?.byte_range_reliable, true);
  });
});
