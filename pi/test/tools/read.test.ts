import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemRead, type SemReadParams } from "../../src/tools/sem-read.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-read-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemReadParams, cwd: string) {
  return performSemRead(params, { cwd, semBin: "sem" });
}

test("read returns the entity's own source via a direct slice when hops is 0 (default)", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts", entity: { name: "standalone" } }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function standalone\(\): number \{\s*\n\s*return 1;\s*\n\}/);

    const details = outcome.details as { range_source?: string; related?: unknown[] };
    assert.equal(details.range_source, "direct-slice");
    assert.equal((details.related ?? []).length, 0);
  });
});

test("read disambiguates an ambiguous name via parent_name, matching weave_edit's rules — refuses without it, succeeds with it", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const ambiguous = await run({ file: "sample.ts", entity: { name: "greet" } }, dir);
    assert.equal(ambiguous.isError, true);
    assert.match(ambiguous.text, /ambiguous/);
    assert.match(ambiguous.text, /Alice/);
    assert.match(ambiguous.text, /Bob/);

    const outcome = await run({ file: "sample.ts", entity: { name: "greet", parent_name: "Bob" } }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /hi from bob/);
    assert.doesNotMatch(outcome.text, /hi from alice/);
  });
});

test("read reports a not-found entity with the closest names instead of an opaque error", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts", entity: { name: "greett" } }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named "greett"/);
    assert.match(outcome.text, /greet/);
  });
});

test("read with hops>0 includes related entities via sem context", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run({ file: "math.ts", entity: { name: "add" }, hops: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function add\(a: number, b: number\)/);
    assert.match(outcome.text, /calculate/); // the related dependent shows up

    const details = outcome.details as { range_source?: string; related?: Array<{ name: string }> };
    assert.equal(details.range_source, "sem-context");
    assert.ok((details.related ?? []).some((r) => r.name === "calculate"));
  });
});

test("read with hops=0 never includes related entities even when some exist", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const outcome = await run({ file: "math.ts", entity: { name: "add" } }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.doesNotMatch(outcome.text, /calculate/);
    const details = outcome.details as { related?: unknown[] };
    assert.equal((details.related ?? []).length, 0);
  });
});

test("read truncates to fit a small budget and reports it", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-read-test-"));
  try {
    const body = Array.from({ length: 40 }, (_, i) => `  const line${i} = ${i};`).join("\n");
    writeFileSync(join(dir, "big.ts"), `export function big() {\n${body}\n  return 0;\n}\n`, "utf8");

    const outcome = await run({ file: "big.ts", entity: { name: "big" }, budget: 10 }, dir);
    assert.equal(outcome.isError, false, outcome.text);

    const details = outcome.details as { truncated?: boolean; budget?: number };
    assert.equal(details.truncated, true);
    assert.equal(details.budget, 10);
    assert.ok(outcome.text.length < 2000, "truncated content should be much shorter than the full 40-line body");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("budget defaults to 1500 when the caller doesn't pass one", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts", entity: { name: "standalone" } }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { budget?: number };
    assert.equal(details.budget, 1500);
  });
});

test("entities= reads several entities in one call instead of one call per entity", async () => {
  await withTempCopy(["math.ts", "sample.ts"], async (dir) => {
    const outcome = await run(
      {
        entities: [
          { name: "add", file: "math.ts" },
          { name: "standalone", file: "sample.ts" },
        ],
      },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function add\(a: number, b: number\)/);
    assert.match(outcome.text, /function standalone\(\): number/);

    const details = outcome.details as { count?: number; results?: Array<{ file?: string; range_source?: string }> };
    assert.equal(details.count, 2);
    assert.equal(details.results?.length, 2);
    assert.equal(details.results?.[0]?.file, "math.ts");
    assert.equal(details.results?.[1]?.file, "sample.ts");
  });
});

test("entities= keeps going and reports a failed lookup inline instead of failing the whole batch", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run(
      {
        entities: [
          { name: "add", file: "math.ts" },
          { name: "totallyMissingName", file: "math.ts" },
        ],
      },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text, "one bad lookup among several should not fail the whole batch");
    assert.match(outcome.text, /function add\(a: number, b: number\)/);
    assert.match(outcome.text, /no entity named "totallyMissingName"/);
  });
});

test("entities= honors a total ~6000-token cap across the whole batch, honestly marking what got skipped", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-read-test-"));
  try {
    // Each file's function body is far bigger than the 1500-token default
    // per-entity budget on its own, so every entity truncates at exactly
    // its own budget regardless of the batch — but 5 of them (5 * 1500 =
    // 7500 tokens) blows well past the ~6000 total cap, so the batch must
    // stop honestly rather than silently rendering all 5 in full.
    for (let i = 0; i < 5; i++) {
      writeFileSync(join(dir, `big${i}.ts`), `export function big${i}(): string {\n  return "${"x".repeat(8000)}";\n}\n`, "utf8");
    }

    const outcome = await run(
      { entities: Array.from({ length: 5 }, (_, i) => ({ name: `big${i}`, file: `big${i}.ts` })) },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);

    const details = outcome.details as { total_tokens?: number; results?: Array<{ truncated?: boolean; skipped?: boolean }> };
    assert.ok((details.total_tokens ?? Infinity) <= 6000, `total_tokens (${details.total_tokens}) must respect the ~6000 batch cap`);
    assert.equal(details.results?.length, 5, "every requested entity must still appear in details, even the ones skipped for budget");
    assert.ok(
      details.results?.some((r) => r.skipped === true || r.truncated === true),
      "at least one entity must be honestly marked as truncated/skipped by the total cap, not silently dropped",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the single entity=/file= form is unaffected by batching — no `results` wrapper leaks into its details", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ file: "sample.ts", entity: { name: "standalone" } }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { results?: unknown; range_source?: string };
    assert.equal(details.results, undefined, "single entity= form must not gain a batch `results` wrapper");
    assert.equal(details.range_source, "direct-slice");
  });
});

test("file is optional: a name that's unique repo-wide resolves and reads in one call", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ entity: { name: "add" } }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function add\(a: number, b: number\)/);
    assert.match(outcome.text, /return a \+ b;/);

    const details = outcome.details as { file?: string };
    assert.equal(details.file, "math.ts", "the file must be auto-discovered and reported");
  });
});

test("file is optional: several repo-wide matches refuse with a candidate list instead of picking one", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-read-test-"));
  try {
    writeFileSync(join(dir, "math.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n", "utf8");
    writeFileSync(join(dir, "other.ts"), "export function add(x: number, y: number): number {\n  return x * y;\n}\n", "utf8");

    const outcome = await run({ entity: { name: "add" } }, dir);
    assert.equal(outcome.isError, true, "must refuse rather than silently pick the first match");
    assert.match(outcome.text, /ambiguous/i);
    assert.match(outcome.text, /math\.ts/);
    assert.match(outcome.text, /other\.ts/);

    const details = outcome.details as { candidates?: Array<{ file?: string; type?: string; start_line?: number; end_line?: number }> };
    assert.equal(details.candidates?.length, 2);
    for (const c of details.candidates ?? []) {
      assert.ok(c.file, "each candidate must report its file");
      assert.ok(c.type, "each candidate must report its type");
      assert.ok(typeof c.start_line === "number" && typeof c.end_line === "number", "each candidate must report its line range");
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("file is optional: a name ambiguous within one file (two methods, same name, different parents) also refuses with candidates", async () => {
  await withTempCopy(["sample.ts"], async (dir) => {
    const outcome = await run({ entity: { name: "greet" } }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /ambiguous/i);
    assert.match(outcome.text, /Alice/);
    assert.match(outcome.text, /Bob/);

    const details = outcome.details as { candidates?: Array<{ parent_name?: string | null }> };
    assert.equal(details.candidates?.length, 2);
    const parents = (details.candidates ?? []).map((c) => c.parent_name).sort();
    assert.deepEqual(parents, ["Alice", "Bob"]);
  });
});

test("file is optional: zero repo-wide matches reports not-found instead of throwing", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ entity: { name: "totallyMissingRepoWide" } }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named "totallyMissingRepoWide"/);
  });
});

test("mode: \"headers\" returns just the signature line, not the body", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run({ file: "math.ts", entity: { name: "add" }, mode: "headers" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function add\(a: number, b: number\): number \{/);
    assert.doesNotMatch(outcome.text, /return a \+ b;/, "headers mode must not include the body");
  });
});

test("mode: \"headers\" includes an immediately-preceding JSDoc summary line", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-read-test-"));
  try {
    writeFileSync(
      join(dir, "doc.ts"),
      ["/**", " * Adds two numbers together.", " */", "export function add(a: number, b: number): number {", "  return a + b;", "}", ""].join("\n"),
      "utf8",
    );
    const outcome = await run({ file: "doc.ts", entity: { name: "add" }, mode: "headers" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /Adds two numbers together\./);
    assert.match(outcome.text, /function add\(a: number, b: number\): number \{/);
    assert.doesNotMatch(outcome.text, /return a \+ b;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("mode: \"headers\" for Python includes the def line plus the docstring's first line", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-read-test-"));
  try {
    writeFileSync(join(dir, "doc.py"), ['def greet(name):', '    """Say hello to someone."""', '    return f"hi {name}"', ""].join("\n"), "utf8");
    const outcome = await run({ file: "doc.py", entity: { name: "greet" }, mode: "headers" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /def greet\(name\):/);
    assert.match(outcome.text, /Say hello to someone\./);
    assert.doesNotMatch(outcome.text, /return f"hi \{name\}"/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("mode: \"headers\" applies uniformly across an entities= batch", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const outcome = await run(
      { entities: [{ name: "add", file: "math.ts" }, { name: "sub", file: "math.ts" }], mode: "headers" },
      dir,
    );
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /function add\(a: number, b: number\): number \{/);
    assert.match(outcome.text, /function sub\(a: number, b: number\): number \{/);
    assert.doesNotMatch(outcome.text, /return a \+ b;/);
    assert.doesNotMatch(outcome.text, /return a - b;/);
  });
});

test("content is required... N/A: read never mutates, but a nonexistent file is reported cleanly", async () => {
  await withTempCopy([], async (dir) => {
    const outcome = await run({ file: "does-not-exist.ts", entity: { name: "anything" } }, dir);
    assert.equal(outcome.isError, true);
  });
});
