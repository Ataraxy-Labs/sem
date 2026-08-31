import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-error-phrasing-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * Next-call error phrasing is a property of EVERY refusal path -- verbs
 * and demoted primitives alike. `toCodeModeError` (api.ts) is the ONE
 * helper every `throw` site in that file routes through -- confirmed by
 * grep in the commit that landed this (26 sites, zero remaining bare
 * `throw new Error(...)`).
 *
 * These tests cover the one behavioral addition beyond the existing
 * tool-name rewrite (already covered by api-mode-aware-messages.test.ts):
 * a "no entity/entities named" refusal now also points at sem.where(),
 * v2 item 1's fuzzy/broad-discovery verb -- the more precise next call
 * for "I don't know the exact spelling" than the underlying tool's own
 * "use sem_grep" suggestion, which predates sem.where() and is shared
 * with tools-mode (which has no sem.where() at all, so that text can't
 * be rewritten to assume it).
 */

// Note: find() itself does NOT throw for zero matches -- sem-find.ts's
// own doc comment establishes "zero matches is a success (isError=false)",
// and it returns a normal {total:0, hits:[]} FindResult. The sem.where()
// augmentation is exercised by verbs that genuinely refuse when nothing
// matches -- read()/callers()/edit() -- covered below via callers() and
// read(), not find().
test("read()'s not-found error points at sem.where() as a broader next call", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.read({ name: "definitelyNotARealName12345" }),
      (err: Error) => {
        assert.match(err.message, /no entity.*named/i);
        assert.match(err.message, /sem\.where/, `expected a sem.where() pointer in the not-found message, got: ${err.message}`);
        return true;
      },
    );
  });
});

test("callers()'s not-found error also points at sem.where()", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.callers("definitelyNotARealName12345"),
      (err: Error) => {
        assert.match(err.message, /sem\.where/, `expected a sem.where() pointer, got: ${err.message}`);
        return true;
      },
    );
  });
});

test("the sem.where() pointer is APPENDED, not a replacement -- the underlying advice is still present", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.callers("definitelyNotARealName12345"),
      (err: Error) => {
        assert.match(err.message, /exact and case-sensitive|check the spelling/i, `expected the underlying tool's own advice to still be present: ${err.message}`);
        return true;
      },
    );
  });
});

test("an error that does NOT mention 'no entity named' is left untouched by the sem.where() augmentation", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.outline("does-not-exist.ts"),
      (err: Error) => {
        assert.doesNotMatch(err.message, /sem\.where/, `sem.where() should only be suggested for a genuine not-found case, got: ${err.message}`);
        return true;
      },
    );
  });
});

test("a constructed (not native-tool-proxied) refusal, like more()'s unknown-handle error, still routes through the same helper (no crash, clean message)", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    assert.throws(() => api.more("h999"), /not a known pagination handle/i);
  });
});

test("no bare `throw new Error(` sites remain in api.ts -- every refusal routes through toCodeModeError", async () => {
  const apiSource = await readFile(join(__dirname, "..", "..", "src", "codemode", "api.ts"), "utf8");
  const bareThrows = apiSource.match(/throw new Error\(/g) ?? [];
  assert.equal(bareThrows.length, 0, `expected zero bare "throw new Error(" sites, found ${bareThrows.length} -- every refusal should route through toCodeModeError`);
  const wrappedThrows = apiSource.match(/throw toCodeModeError\(/g) ?? [];
  assert.ok(wrappedThrows.length >= 20, `expected at least 20 throw sites routed through toCodeModeError, found ${wrappedThrows.length}`);
});
