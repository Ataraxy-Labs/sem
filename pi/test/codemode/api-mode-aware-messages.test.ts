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
  const dir = mkdtempSync(join(tmpdir(), "codemode-mode-aware-messages-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * Review pass 3 (Ox's late report), item 3: every native tool api.ts wraps
 * (performSemRead/Find/Grep/Outline/Callers/WeaveEdit) prefixes its own
 * error text with its OWN tool name -- correct in tools-mode, where that's
 * the registered tool name, but api.ts ONLY EVER runs in code mode, which
 * registers exactly one tool (sem_code) and none of sem_read/sem_find/
 * sem_grep/sem_outline/sem_callers/weave_edit at all. Confirmed live before
 * fixing: `api.edit({entity:{name:"doesNotExist"}})` threw
 * "weave_edit: no entity named..." verbatim -- telling the model to reach
 * for a tool it doesn't have. Fixed via api.ts's toCodeModeMessage(),
 * applied at every one of the 8 `throw new Error(outcome.text)` boundary
 * crossings in that file.
 */
test("edit()'s not-found error says sem.edit, never weave_edit", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.edit({ file: "calls.ts", entity: { name: "doesNotExistAtAll12345" }, op: "replace", content: "x" }),
      (err: Error) => {
        assert.match(err.message, /sem\.edit/, `expected "sem.edit" in the message, got: ${err.message}`);
        assert.doesNotMatch(err.message, /\bweave_edit\b/, `must never mention the tools-mode tool name: ${err.message}`);
        return true;
      },
    );
  });
});

test("read()'s not-found error says sem.read, never sem_read", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.read({ name: "doesNotExistAtAll12345", file: "calls.ts" }),
      (err: Error) => {
        assert.match(err.message, /sem\.read/, err.message);
        assert.doesNotMatch(err.message, /\bsem_read\b/, err.message);
        return true;
      },
    );
  });
});

test("find()'s missing-argument error says sem.find, never sem_find", async () => {
  const api = buildSemApi({ cwd: process.cwd(), semBin: "sem" });
  // find() always passes query/queries through -- force the "missing
  // query" refusal via an empty array, which performSemFind treats the
  // same as neither being passed.
  await assert.rejects(
    () => api.find([]),
    (err: Error) => {
      assert.match(err.message, /sem\.find/, err.message);
      assert.doesNotMatch(err.message, /\bsem_find\b/, err.message);
      return true;
    },
  );
});

test("callers()'s not-found error says sem.callers and sem.find, never sem_callers/sem_find", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.callers("doesNotExistAtAll12345"),
      (err: Error) => {
        assert.match(err.message, /sem\.callers/, err.message);
        // sem_callers's own guidance mentions sem_find as a follow-up --
        // that mention needs the SAME translation, not just the leading prefix.
        assert.doesNotMatch(err.message, /\bsem_callers\b/, err.message);
        assert.doesNotMatch(err.message, /\bsem_find\b/, err.message);
        return true;
      },
    );
  });
});

test("outline()'s error (via headers(file)) says sem.outline, never sem_outline", async () => {
  const api = buildSemApi({ cwd: process.cwd(), semBin: "sem" });
  await assert.rejects(
    () => api.outline("this/file/does/not/exist/at/all.ts"),
    (err: Error) => {
      assert.match(err.message, /sem\.outline/, err.message);
      assert.doesNotMatch(err.message, /\bsem_outline\b/, err.message);
      return true;
    },
  );
});
