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
  const dir = mkdtempSync(join(tmpdir(), "codemode-where-explain-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

// calls.ts: add(a,b) defined once; twice(n) and thrice(n) both call add(...).

/**
 * v2 item 1: where() answers "where does this concept live" as ONE ranked,
 * deduped call -- exact-name definitions (find) ∪ full-text mentions
 * (grep) -- for the fuzzy/half-remembered-name case find() alone can't
 * serve (sem_find's own contract: no substring/fuzzy matching).
 */
test("where(): a real definition is ranked first, as a 'definition' row", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.where("add")) as { concept: string; total: number; rows: Array<{ name: string; file: string; kind: string; h: string }> };
    assert.equal(result.concept, "add");
    assert.ok(result.rows.length > 0);
    assert.equal(result.rows[0]!.kind, "definition", `expected the definition ranked first, got: ${JSON.stringify(result.rows)}`);
    assert.equal(result.rows[0]!.name, "add");
    assert.match(result.rows[0]!.h, /^h\d+$/);
  });
});

test("where(): text mentions at OTHER call sites come back as 'reference' rows", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.where("add")) as { rows: Array<{ file: string; kind: string; line: number }> };
    const references = result.rows.filter((r) => r.kind === "reference");
    assert.ok(references.length >= 2, `expected at least 2 reference rows (twice() and thrice() both call add), got: ${JSON.stringify(result.rows)}`);
  });
});

test("where(): the definition's OWN line is not ALSO reported as a redundant 'reference' row", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.where("add")) as { rows: Array<{ kind: string; line: number }> };
    const definitionLine = result.rows.find((r) => r.kind === "definition")!.line;
    const referenceOnSameLine = result.rows.find((r) => r.kind === "reference" && r.line === definitionLine);
    assert.equal(referenceOnSameLine, undefined, `the definition's own line should not also show up as a 'reference': ${JSON.stringify(result.rows)}`);
  });
});

test("where(): a concept with zero find matches still returns grep-only rows, without throwing", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    // "Doubles" only appears in a comment above twice() -- no entity is
    // named that, so find() contributes nothing, but grep() should still
    // surface the comment as a reference.
    const result = (await api.where("Doubles")) as { rows: Array<{ kind: string }> };
    assert.ok(result.rows.every((r) => r.kind === "reference"));
    assert.ok(result.rows.length >= 1, `expected at least the comment mention, got: ${JSON.stringify(result.rows)}`);
  });
});

/**
 * v2 item 1: explain() answers "what is this and who uses it" as ONE call
 * -- signature + doc + usage + a short deterministic summary -- instead
 * of a model chaining headers()/read() + callers() itself.
 */
test("explain(): returns signature, doc, usage, and a paragraph mentioning the caller count", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.explain({ name: "add", file: "calls.ts" })) as {
      name: string;
      type: string;
      file: string;
      signature: string;
      doc: string | null;
      usage: Array<{ name: string; file: string }>;
      usage_count: number | null;
      paragraph: string;
    };
    assert.equal(result.name, "add");
    assert.equal(result.type, "function");
    assert.match(result.signature, /function add/);
    assert.equal(result.usage_count, 2, `expected 2 callers (twice, thrice), got: ${JSON.stringify(result.usage)}`);
    assert.ok(result.usage.some((u) => u.name === "twice"));
    assert.ok(result.usage.some((u) => u.name === "thrice"));
    assert.match(result.paragraph, /2 callers/);
    assert.match(result.paragraph, /calls\.ts/);
  });
});

test("explain(): accepts an h<n> handle from an earlier find() row, not just a locator", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const found = (await api.find("twice")) as { hits: Array<{ h: string }> };
    const result = (await api.explain(found.hits[0]!.h)) as { name: string };
    assert.equal(result.name, "twice");
  });
});

test("explain(): a doc comment above the entity is captured", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.explain({ name: "twice", file: "calls.ts" })) as { doc: string | null; paragraph: string };
    assert.match(result.doc ?? "", /doubles a number/i);
    assert.match(result.paragraph, /doubles a number/i);
  });
});
