import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemOutline, type SemOutlineParams } from "../../src/tools/sem-outline.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

// IMPORTANT: sem 0.23.0 silently omits byte ranges for ANY file (code
// included) whose path contains a "fixtures" path segment, even with
// --no-default-excludes — confirmed empirically, distinct from the
// documented non-code-entity byte-range gap. Fixtures MUST be copied to a
// non-"fixtures"-named tmpdir before exercising sem's byte-range behavior,
// or every test would see byteRangeReliable: false regardless of language.
function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-outline-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemOutlineParams, cwd: string) {
  return performSemOutline(params, { cwd, semBin: "sem" });
}

test("outline nests classes/methods under their parent, in file order", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const lines = outcome.text.split("\n");
    assert.match(lines[0] ?? "", /^sample\.ts: 6 entities, ~63 tokens$/);

    const aliceIdx = lines.findIndex((l) => l.includes("class Alice"));
    const bobIdx = lines.findIndex((l) => l.includes("class Bob"));
    const standaloneIdx = lines.findIndex((l) => l.includes("function standalone"));
    assert.ok(aliceIdx > 0 && bobIdx > aliceIdx && standaloneIdx > bobIdx, "top-level entities appear in file order");

    // The two `greet` methods must each be indented under their own class,
    // immediately following it, not merged or reordered.
    const aliceGreetLine = lines[aliceIdx + 1] ?? "";
    assert.match(aliceGreetLine, /^\s{2,}method greet/);
    const bobGreetLine = lines[bobIdx + 1] ?? "";
    assert.match(bobGreetLine, /^\s{2,}method greet/);

    // Top-level lines are NOT indented.
    assert.equal(/^\S/.test(lines[aliceIdx] ?? ""), true);
  });
});

// Per-entity ~token counts below are intentionally NOT pinned to exact
// numbers: whether sem returns a byte range at all for a given entity kind
// is sem-version-dependent (confirmed empirically: sem 0.23.0 omits byte
// ranges for non-code entities like markdown/toml; 0.23.1+ includes one),
// and even for code entities sem's own byte-counting has drifted slightly
// between versions. The actual guarantee these tests exist to protect —
// byte-derived vs line-count-derived token estimates, and which shape
// produces which — is precisely and deterministically covered by
// entities.test.ts's deriveEntity tests and
// sem-outline-token-estimate.test.ts's tokenEstimate tests, which feed
// captured JSON shapes through the same code directly. These stay as live
// smokes over the real sem binary: structural correctness (right entity,
// right line range, right nesting, a positive ~N appears, byte_range_reliable
// is present as a boolean) without pinning a specific sem version's exact
// numbers.
test("outline reports a byte-range-reliable token estimate for ordinary code entities", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function standalone \[L13-L15\] ~\d+/);

    const details = outcome.details as { entities?: Array<{ name: string; byte_range_reliable: boolean }> };
    const standalone = details.entities?.find((e) => e.name === "standalone");
    assert.equal(typeof standalone?.byte_range_reliable, "boolean");
  });
});

test("outline works on markdown headings, including ones with disjoint nested line ranges", async () => {
  await withTempCopy(["notes.md"], async (dir) => {
    const outcome = await run({ file: "notes.md" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const lines = outcome.text.split("\n");
    assert.match(lines[0] ?? "", /^notes\.md: 3 entities, ~\d+ tokens$/);
    assert.match(outcome.text, /heading Section One \[L5-L9\] ~\d+/);

    const notesIdx = lines.findIndex((l) => l.includes("heading Notes"));
    const sectionOneIdx = lines.findIndex((l) => l.includes("Section One"));
    assert.ok(notesIdx >= 0 && sectionOneIdx > notesIdx);
    assert.match(lines[sectionOneIdx] ?? "", /^\s{2,}heading Section One/, "Section One nests under Notes despite disjoint line ranges");

    const details = outcome.details as { entities?: Array<{ name: string; byte_range_reliable: boolean }> };
    const sectionOne = details.entities?.find((e) => e.name === "Section One");
    assert.equal(typeof sectionOne?.byte_range_reliable, "boolean");
  });
});

test("outline works on toml sections", async () => {
  await withTempCopy(["config.toml"], async (dir) => {
    const outcome = await run({ file: "config.toml" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /^config\.toml: 2 entities, ~\d+ tokens$/m);
    assert.match(outcome.text, /section server \[L1-L3\] ~\d+/);
    assert.match(outcome.text, /section database \[L5-L7\] ~\d+/);
  });
});

test("text= filters to matching entities while keeping ancestor context, without changing header totals", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts", text: "greet" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    // Header still reports the FULL file's stats, plus a note about the filter.
    assert.match(outcome.text, /^sample\.ts: 6 entities, ~63 tokens \(matching "greet": \d+ shown\)$/m);

    assert.match(outcome.text, /class Alice/); // ancestor context kept
    assert.match(outcome.text, /class Bob/);
    assert.match(outcome.text, /method greet/);
    assert.doesNotMatch(outcome.text, /function standalone/); // no match, no ancestor relevance -> dropped
    assert.doesNotMatch(outcome.text, /function helper/);
  });
});

test("depth caps how deep the tree renders without changing the reported entity count", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts", depth: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /^sample\.ts: 6 entities, ~63 tokens/);
    assert.match(outcome.text, /class Alice/);
    assert.doesNotMatch(outcome.text, /method greet/); // depth 1 = top-level only
  });
});

test("output caps at ~120 lines with a count of the rest, suggesting text= or depth=", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-outline-test-"));
  try {
    const lines: string[] = [];
    for (let i = 0; i < 200; i++) {
      lines.push(`export function fn${i}(): number {`, `  return ${i};`, `}`, "");
    }
    writeFileSync(join(dir, "big.ts"), lines.join("\n"), "utf8");

    const outcome = await run({ file: "big.ts" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const outputLines = outcome.text.split("\n");
    const entityLines = outputLines.filter((l) => /^fn\d+ |function fn\d+/.test(l.trim()));
    assert.ok(entityLines.length <= 120, `expected <= 120 rendered entity lines, got ${entityLines.length}`);
    assert.match(outcome.text, /…\d+ more \(use text= or depth=\)/);

    const details = outcome.details as { truncated?: boolean };
    assert.equal(details.truncated, true);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("when the cap would be exceeded, whole nesting levels collapse first rather than an arbitrary mid-tree cutoff", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-outline-test-"));
  try {
    // 70 top-level classes x 1 method each = 140 entities. Rendering
    // everything (depth 2) is 140 lines, over the ~120 cap. Rendering only
    // the top level (depth 1) is 70 lines, comfortably under it. The
    // deepest-first strategy must prefer showing ALL 70 classes with their
    // methods collapsed over an arbitrary pre-order cut that would show
    // some classes' methods while dropping other classes entirely.
    const parts: string[] = [];
    for (let i = 0; i < 70; i++) {
      parts.push(`export class C${i} {\n  m${i}(): number {\n    return ${i};\n  }\n}\n`);
    }
    writeFileSync(join(dir, "wide.ts"), parts.join("\n"), "utf8");

    const outcome = await run({ file: "wide.ts" }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    for (let i = 0; i < 70; i++) {
      assert.match(outcome.text, new RegExp(`class C${i}\\b`), `class C${i} should still be shown — only depth should collapse, not coverage`);
    }
    assert.doesNotMatch(outcome.text, /method m\d+/, "methods (the deepest level) should be collapsed, not partially shown");
    assert.match(outcome.text, /…70 more \(use text= or depth=\)/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
