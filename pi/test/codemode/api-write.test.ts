import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-api-write-test-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("write() creates a brand-new file", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.write("brand-new.ts", "export const x = 1;\n");
    assert.equal(result.path, "brand-new.ts");
    assert.equal(readFileSync(join(dir, "brand-new.ts"), "utf8"), "export const x = 1;\n");
  });
});

test("write() refuses to overwrite an existing NON-code file unless overwrite: true", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "notes.txt"), "one\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    await assert.rejects(() => api.write("notes.txt", "two\n"), /overwrite|exist/i);
    assert.equal(readFileSync(join(dir, "notes.txt"), "utf8"), "one\n");

    await api.write("notes.txt", "two\n", { overwrite: true });
    assert.equal(readFileSync(join(dir, "notes.txt"), "utf8"), "two\n");
  });
});

// A `.ts` file is a code file per write-audit.ts's isCodeFilePath -- an
// EXISTING one carries a stricter rule than the non-code case above (see
// test/codemode/api-write-bypasses-edit-safety.review.test.ts): plain
// `{ overwrite: true }` is refused; only `{ overwrite: "force" }` bypasses
// it, and even that is refused outright under PI_SEM_STRICT.
test("write() refuses to overwrite an EXISTING code file even with overwrite: true -- use sem.edit for those", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "existing.ts"), "export const x = 1;\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    await assert.rejects(() => api.write("existing.ts", "export const x = 2;\n"), /use sem\.edit for existing entities/);
    await assert.rejects(() => api.write("existing.ts", "export const x = 2;\n", { overwrite: true }), /use sem\.edit for existing entities/);
    assert.equal(readFileSync(join(dir, "existing.ts"), "utf8"), "export const x = 1;\n");

    await api.write("existing.ts", "export const x = 2;\n", { overwrite: "force" });
    assert.equal(readFileSync(join(dir, "existing.ts"), "utf8"), "export const x = 2;\n");
  });
});
