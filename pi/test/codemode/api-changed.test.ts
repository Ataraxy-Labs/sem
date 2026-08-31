import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, createChangeLog } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-changed-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * changed() answers "what have I touched this session" by reading back the
 * SAME session-scoped ChangeLog every edit()/write() call already records
 * into -- SESSION-scoped for the identical reason handles are: a model
 * closing out a task asks this in a LATER sem_code call, not just within
 * one script.
 */
test("changed() is empty before any edit()/write() call", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = api.changed() as { count: number; files: string[]; entries: unknown[] };
    assert.equal(result.count, 0);
    assert.deepEqual(result.files, []);
    assert.deepEqual(result.entries, []);
  });
});

test("changed() records a single edit() call", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.edit({
      file: "calls.ts",
      entity: { name: "add" },
      op: "replace",
      content: "export function add(a: number, b: number): number {\n  return a + b + 0;\n}",
    });
    const result = api.changed() as { count: number; files: string[]; entries: Array<{ file: string; entity?: string; op: string; at: number }> };
    assert.equal(result.count, 1);
    assert.deepEqual(result.files, ["calls.ts"]);
    assert.equal(result.entries[0]!.file, "calls.ts");
    assert.equal(result.entries[0]!.entity, "add");
    assert.equal(result.entries[0]!.op, "replace");
    assert.ok(typeof result.entries[0]!.at === "number" && result.entries[0]!.at > 0);
  });
});

test("changed() records a write() call", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.write("new-file.ts", "export const x = 1;\n");
    const result = api.changed() as { count: number; files: string[]; entries: Array<{ file: string; op: string }> };
    assert.equal(result.count, 1);
    assert.deepEqual(result.files, ["new-file.ts"]);
    assert.equal(result.entries[0]!.op, "write");
  });
});

test("changed() groups multiple edits to the same file under ONE file entry, but every entry is still recorded", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.edit({
      file: "calls.ts",
      entity: { name: "add" },
      op: "replace",
      content: "export function add(a: number, b: number): number {\n  return a + b + 0;\n}",
    });
    await api.edit({
      file: "calls.ts",
      entity: { name: "twice" },
      op: "replace",
      content: "export function twice(n: number): number {\n  return add(n, n) + 0;\n}",
    });
    const result = api.changed() as { count: number; files: string[]; entries: unknown[] };
    assert.equal(result.count, 2, "both edits recorded as separate entries");
    assert.deepEqual(result.files, ["calls.ts"], "one file, deduped");
  });
});

test("changed() does NOT record a REFUSED edit -- only successful mutations count", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.edit({ file: "calls.ts", entity: { name: "does_not_exist" }, op: "replace", content: "x" }));
    const result = api.changed() as { count: number };
    assert.equal(result.count, 0, "a refused edit should not appear in changed()");
  });
});

test("changed() is SESSION-scoped: an edit() in one buildSemApi() call is visible via changed() in a LATER buildSemApi() call sharing the same ChangeLog", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const changes = createChangeLog();

    // Call 1: mirrors one sem_code tool invocation.
    const apiCall1 = buildSemApi({ cwd: dir, semBin: "sem", changes });
    await apiCall1.write("touched-in-call-1.ts", "export const y = 1;\n");

    // Call 2: a SEPARATE buildSemApi() construction sharing the SAME
    // changes instance -- the write from call 1 must be visible here.
    const apiCall2 = buildSemApi({ cwd: dir, semBin: "sem", changes });
    const result = apiCall2.changed() as { count: number; files: string[] };
    assert.equal(result.count, 1);
    assert.deepEqual(result.files, ["touched-in-call-1.ts"], "a change recorded in an earlier sem_code call must be visible via changed() in a later one within the same session");
  });
});

test("changed() is isolated by DEFAULT across separate buildSemApi() constructions that don't share a ChangeLog", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const apiA = buildSemApi({ cwd: dir, semBin: "sem" });
    await apiA.write("only-in-a.ts", "export const z = 1;\n");

    const apiB = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = apiB.changed() as { count: number };
    assert.equal(result.count, 0, "without an explicitly shared ChangeLog, each buildSemApi() call starts with its own empty log");
  });
});
