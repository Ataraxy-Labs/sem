import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { buildSemApi } from "../../src/codemode/api.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-field-completeness-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function git(dir: string, ...args: string[]): void {
  execFileSync("git", args, { cwd: dir, stdio: "ignore" });
}

/** A minimal git repo with one committed file, then an UNCOMMITTED edit -- enough for `sem diff` (default: working tree vs HEAD) to have real changes to report. */
function withGitDiffFixture<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-field-completeness-diff-test-"));
  return (async () => {
    git(dir, "init", "-q");
    git(dir, "config", "user.email", "test@example.com");
    git(dir, "config", "user.name", "Test");
    writeFileSync(join(dir, "calc.ts"), "export function add(a: number, b: number): number {\n  return a + b;\n}\n", "utf8");
    git(dir, "add", "calc.ts");
    git(dir, "commit", "-q", "-m", "initial");
    // Modify the committed function (uncommitted) and add a brand-new one.
    writeFileSync(
      join(dir, "calc.ts"),
      "export function add(a: number, b: number): number {\n  return a + b + 0;\n}\n\nexport function sub(a: number, b: number): number {\n  return a - b;\n}\n",
      "utf8",
    );
    return run(dir);
  })().finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * An exact live repro: a live model's chained
 * `sem.callers("add")` -> `sem.headers(callers.callers.map(...))` came back
 * with rows holding ONLY `file` -- the sem-api.d.ts's HeaderLine promises
 * name/type/parent_name/signature/doc too, and api.ts silently didn't
 * deliver them. Root cause: sem_read's mode="headers" `details` nests
 * {name,type,parent_name} under `.entity` and never exposes signature/doc
 * as fields at all (only merged into rendered `.text`) -- headers() was
 * spreading that raw shape instead of reshaping it to what the d.ts
 * promises. Fixed in api.ts's headerLine().
 */
test("callers()->headers() chain returns fully-populated rows on both ends (exact live repro)", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const callers = (await api.callers("add")) as {
      callers: Array<{ name: string; type: string; file: string; start_line: number; end_line: number }>;
    };

    assert.equal(callers.callers.length, 2, `expected twice+thrice as callers of add, got ${JSON.stringify(callers)}`);
    for (const c of callers.callers) {
      assert.ok(c.name, `caller missing name: ${JSON.stringify(c)}`);
      assert.ok(c.type, `caller missing type: ${JSON.stringify(c)}`);
      assert.ok(c.file, `caller missing file: ${JSON.stringify(c)}`);
      assert.ok(typeof c.start_line === "number" && c.start_line > 0, `caller missing start_line: ${JSON.stringify(c)}`);
    }
    assert.deepEqual(
      callers.callers.map((c) => c.name).sort(),
      ["thrice", "twice"],
    );

    const locators = callers.callers.map((c) => ({ name: c.name, file: c.file, entity_type: c.type }));
    const headers = (await api.headers(locators)) as Array<{
      name: string;
      type: string;
      file: string;
      parent_name: string | null;
      signature: string;
      doc: string | null;
    }>;

    assert.equal(headers.length, 2);
    for (const h of headers) {
      assert.ok(h.name, `header row missing name -- got only: ${JSON.stringify(h)}`);
      assert.ok(h.type, `header row missing type -- got only: ${JSON.stringify(h)}`);
      assert.ok(h.file, `header row missing file -- got only: ${JSON.stringify(h)}`);
      assert.ok("parent_name" in h, `header row missing parent_name key entirely -- got only: ${JSON.stringify(h)}`);
      assert.ok(h.signature && h.signature.length > 0, `header row missing signature -- got only: ${JSON.stringify(h)}`);
      assert.ok("doc" in h, `header row missing doc key entirely -- got only: ${JSON.stringify(h)}`);
    }

    const twiceHeader = headers.find((h) => h.name === "twice");
    const thriceHeader = headers.find((h) => h.name === "thrice");
    assert.ok(twiceHeader, "expected a header row for twice");
    assert.ok(thriceHeader, "expected a header row for thrice");
    assert.match(twiceHeader!.signature, /function twice/);
    assert.match(thriceHeader!.signature, /function thrice/);
    assert.equal(twiceHeader!.doc, "Doubles a number by adding it to itself.", "the // comment immediately above twice should surface as doc");
    assert.equal(thriceHeader!.parent_name, null);
    assert.equal(thriceHeader!.doc, null, "thrice has no preceding comment -- doc must be null, not missing/undefined");
  });
});

test("headers(file) -- the whole-file form -- is equally complete, not just the entities[] form", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const headers = (await api.headers("calls.ts")) as Array<{ name: string; type: string; file: string; signature: string; doc: string | null }>;

    assert.equal(headers.length, 3);
    for (const h of headers) {
      assert.ok(h.name);
      assert.ok(h.type);
      assert.equal(h.file, "calls.ts");
      assert.ok(h.signature && h.signature.length > 0);
    }
  });
});

test("read()'s entity carries `file`, matching the EntitySummary shape the d.ts promises, not just a bare entity", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.read({ name: "add", file: "calls.ts" })) as { entity: { name: string; type: string; file: string; parent_name: string | null; start_line: number; end_line: number } };
    assert.equal(result.entity.name, "add");
    assert.equal(result.entity.file, "calls.ts", "entity.file must be populated, not missing");
    assert.ok(typeof result.entity.start_line === "number" && result.entity.start_line > 0);
  });
});

// Dogfood round 2, finding 2: read()'s `content` used to be
// performSemRead's DECORATED report (`outcome.text` -- a "sem_read: ...
// lines N-M" header, an optional related-entities line, the body, then a
// "[source: ...]" footer), not the entity's bare source. A real dogfood
// script did the natural thing -- `read.content.trimEnd() + "\n\n" + docs`
// then `sem.edit(..., content)` to add a doc comment above a constant --
// which spliced the WHOLE decorated report into the file in place of the
// entity. The result was a syntactically broken region; re-extraction found
// no entity there at all, and weave_edit's identity check reported a
// confusing "changes its name — name changed from X to (no entity found at
// the edited location)" refusal that had nothing to do with the actual
// defect (garbled written content, not a genuine rename). This looked like
// the same class of bug 6bbb809 fixed (post-edit entity re-anchoring after
// a shifted start_line) but is a different root cause entirely: a
// field-completeness bug in what read() hands the SCRIPT to work with.
test("read()'s content is the entity's bare source, not performSemRead's decorated header+footer report", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.read({ name: "add", file: "calls.ts" })) as { content: string };

    assert.equal(
      result.content,
      "export function add(a: number, b: number): number {\n  return a + b;\n}",
      `content must be exactly the entity's own source, no header/footer decoration. Got: ${JSON.stringify(result.content)}`,
    );
    assert.doesNotMatch(result.content, /^sem_read:/, "content must not start with performSemRead's report header");
    assert.doesNotMatch(result.content, /\[source: /, "content must not carry performSemRead's report footer");
  });
});

// The literal dogfood repro: read an entity, append a doc comment to its
// (now-bare) content, feed the result straight back into sem.edit() -- the
// exact script shape from results/t1-code2.jsonl.raw. Before the fix, this
// spliced the decorated report into the file and the edit was refused with
// a false "changes its name" identity error; after the fix, the doc comment
// lands cleanly above valid, unbroken source.
test("read().content -> append a doc comment -> edit() round-trips cleanly (the literal dogfood repro)", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const read = (await api.read({ name: "add", file: "calls.ts" })) as { content: string };
    const docs = "/// Adds two numbers together.";
    const content = docs + "\n" + read.content.trimEnd();

    const result = (await api.edit({ file: "calls.ts", entity: { name: "add" }, op: "replace", content })) as { error?: string };
    assert.equal(result.error, undefined, `edit must not be refused. Got: ${JSON.stringify(result)}`);

    const fileText = readFileSync(join(dir, "calls.ts"), "utf8");
    assert.match(fileText, /\/\/\/ Adds two numbers together\.\nexport function add/);
  });
});

// Prove field completeness survives the ACTUAL sandbox JSON round-trip
// (vm.compileFunction trampoline + JSON.stringify/parse), not just api.ts's
// own return value before the sandbox ever touches it -- the original bug
// was found via a live sandboxed run, so this is the real arbiter.
test("e2e: the callers->headers chain is fully populated after a real sandboxed run, not just at the api.ts layer", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const script = `
      const c = await sem.callers("add");
      const h = await sem.headers(c.callers.map(x => ({ name: x.name, file: x.file, entity_type: x.type })));
      return h;
    `;
    const result = await runInSandbox(script, { sem: api });
    assert.equal(result.ok, true, JSON.stringify(result));

    const rows = result.value as Array<Record<string, unknown>>;
    assert.equal(rows.length, 2, `expected 2 header rows, got ${JSON.stringify(rows)}`);
    for (const row of rows) {
      const keys = Object.keys(row).sort();
      assert.deepEqual(keys, ["doc", "file", "name", "parent_name", "signature", "type"], `row has the wrong field set: ${JSON.stringify(row)}`);
      assert.ok(row.name, `row.name empty: ${JSON.stringify(row)}`);
      assert.ok(row.type, `row.type empty: ${JSON.stringify(row)}`);
      assert.ok(row.file, `row.file empty: ${JSON.stringify(row)}`);
      assert.ok(row.signature, `row.signature empty: ${JSON.stringify(row)}`);
    }
  });
});

// --- review pass 3, item 3: the SAME audit re-run over find/grep/impact/diff ---
// (sem-api.d.ts vs. what each function actually returns, at both the api.ts
// layer and through a real sandboxed run).

test("find(): single-query result carries `hits` (not `results`), matching sem-api.d.ts's FindResult exactly", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.find("add")) as { query: string; type: string | null; total: number; shown: number; hits: Array<Record<string, unknown>> };
    assert.equal(result.query, "add");
    assert.ok("type" in result, "FindResult must carry a type key even when no type filter was given");
    assert.equal(typeof result.total, "number");
    assert.equal(typeof result.shown, "number");
    assert.ok(Array.isArray(result.hits), `expected a hits[] array, got: ${JSON.stringify(result)}`);
    assert.ok(!("results" in result), "FindResult must not also carry the old, wrong `results` field name");
    const hit = result.hits[0]!;
    assert.equal(hit.name, "add");
    assert.ok(hit.file, `hit missing file: ${JSON.stringify(hit)}`);
  });
});

test("find(names[]): batch call returns a SINGLE FindBatchResult object (results: FindResult[]), not a bare array", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.find(["add", "twice"])) as { total_queries: number; ran: number; omitted: number; results: Array<Record<string, unknown>> };
    assert.equal(result.total_queries, 2);
    assert.equal(result.ran, 2);
    assert.equal(result.omitted, 0);
    assert.equal(result.results.length, 2);
    for (const r of result.results) {
      assert.ok("hits" in r, `each batch entry must itself be a FindResult with hits[]: ${JSON.stringify(r)}`);
    }
  });
});

test("grep(): single-pattern result carries path/glob/context/shown, not just pattern/total/hits", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.grep("function add")) as { pattern: string; path: unknown; glob: unknown; context: unknown; total: number; shown: number; hits: unknown[] };
    assert.equal(result.pattern, "function add");
    assert.ok("path" in result, `GrepResult missing path key entirely: ${JSON.stringify(result)}`);
    assert.ok("glob" in result, `GrepResult missing glob key entirely: ${JSON.stringify(result)}`);
    assert.ok("context" in result, `GrepResult missing context key entirely: ${JSON.stringify(result)}`);
    assert.equal(typeof result.shown, "number", `GrepResult missing shown: ${JSON.stringify(result)}`);
    assert.ok(Array.isArray(result.hits) && result.hits.length > 0);
  });
});

// Review pass 3, item 4: grep(patterns[]) previously unwrapped to a bare
// array, silently discarding total_patterns/ran/omitted -- upgraded to the
// SAME wrapper-object shape as find(names[]), rather than the reverse
// (which would have thrown away real information just for shape parity).
// A .meta-on-array alternative was considered and ruled out: an array's
// non-index properties never survive JSON.stringify (verified directly),
// and every value crossing the sandbox boundary is JSON-round-tripped.
test("grep(patterns[]): batch call returns a SINGLE GrepBatchResult object (results: GrepResult[]), like find(), not a bare array", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.grep(["function add", "function twice"])) as {
      total_patterns: number;
      ran: number;
      omitted: number;
      results: Array<{ pattern: string; hits: unknown[] }>;
    };
    assert.equal(result.total_patterns, 2);
    assert.equal(result.ran, 2);
    assert.equal(result.omitted, 0);
    assert.equal(result.results.length, 2);
    assert.equal(result.results[0]?.pattern, "function add");
    assert.equal(result.results[1]?.pattern, "function twice");
    for (const r of result.results) {
      assert.ok("hits" in r, `each batch entry must itself be a GrepResult with hits[]: ${JSON.stringify(r)}`);
    }
  });
});

test("impact(): every EntitySummary in dependencies/dependents/transitive_impact/affected_tests is fully populated", async () => {
  const api = buildSemApi({ cwd: process.cwd(), semBin: "sem" });
  const result = (await api.impact("buildSemApi")) as {
    entity: string;
    dependencies: Array<Record<string, unknown>>;
    dependents: Array<Record<string, unknown>>;
    transitive_impact: Array<Record<string, unknown>>;
    affected_tests: Array<Record<string, unknown>>;
  };
  assert.equal(result.entity, "buildSemApi");
  const groups = [result.dependencies, result.dependents, result.transitive_impact, result.affected_tests];
  assert.ok(groups.some((g) => g.length > 0), `expected at least one non-empty impact group for buildSemApi, got all empty: ${JSON.stringify(result)}`);
  for (const group of groups) {
    for (const entry of group) {
      assert.deepEqual(
        Object.keys(entry).sort(),
        ["end_line", "file", "name", "parent_name", "start_line", "type"],
        `impact entry has the wrong field set: ${JSON.stringify(entry)}`,
      );
    }
  }
});

test("diff(): each change entry carries name/type/file/change -- NOT the raw sem CLI's entityName/entityType/filePath/changeType", async () => {
  await withGitDiffFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.diff()) as { ref: string | null; changes: Array<Record<string, unknown>> };
    assert.equal(result.ref, null);
    assert.ok(result.changes.length > 0, `expected at least one change in the diff fixture, got: ${JSON.stringify(result)}`);
    for (const change of result.changes) {
      assert.ok(typeof change.name === "string" && change.name.length > 0, `change missing name (still shaped like the raw CLI's entityName?): ${JSON.stringify(change)}`);
      assert.ok(typeof change.type === "string" && change.type.length > 0, `change missing type: ${JSON.stringify(change)}`);
      assert.ok(typeof change.file === "string" && change.file.length > 0, `change missing file: ${JSON.stringify(change)}`);
      assert.ok(typeof change.change === "string" && change.change.length > 0, `change missing change: ${JSON.stringify(change)}`);
      assert.ok(!("entityName" in change) && !("entityType" in change) && !("filePath" in change) && !("changeType" in change), `change still leaks the raw CLI's field names: ${JSON.stringify(change)}`);
    }
    const addChange = result.changes.find((c) => c.name === "add");
    assert.ok(addChange, `expected a change entry for the modified 'add' function, got: ${JSON.stringify(result.changes)}`);
    assert.equal(addChange!.file, "calc.ts");
  });
});

// Prove diff()'s field completeness survives the real sandbox round-trip too.
test("e2e: diff() results are fully populated after a real sandboxed run", async () => {
  await withGitDiffFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await runInSandbox("return await sem.diff();", { sem: api });
    assert.equal(result.ok, true, JSON.stringify(result));
    const value = result.value as { changes: Array<Record<string, unknown>> };
    assert.ok(value.changes.length > 0);
    for (const change of value.changes) {
      assert.ok(change.name, `sandboxed diff() change missing name: ${JSON.stringify(change)}`);
      assert.ok(change.file, `sandboxed diff() change missing file: ${JSON.stringify(change)}`);
      assert.ok(change.change, `sandboxed diff() change missing change: ${JSON.stringify(change)}`);
    }
  });
});
