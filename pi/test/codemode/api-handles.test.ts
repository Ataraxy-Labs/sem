import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, createHandleStore } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-handles-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * v2 item 3 (handles): every row a verb returns carries an `h<n>` id, and
 * any verb that accepts an entity locator/name also accepts a handle from
 * an earlier result -- so a script never has to re-type a file path or
 * re-resolve a name it already saw. First slice: find()'s and callers()'s
 * rows get handles; read() accepts them.
 *
 * SESSION-scoped, not run-scoped: a handle minted in one `sem_code` call
 * must still resolve in a LATER call within the same pi session -- that's
 * the whole point (the
 * measured habit is re-grepping the same thing in a later call, not twice
 * in one script). `buildSemApi()` itself defaults to a FRESH, call-local
 * `HandleStore` when `deps.handles` isn't given (tests below, or a direct
 * non-sandboxed caller) -- isolated by default. Sharing is opt-in: pass the
 * SAME `HandleStore` (via `SemApiDeps.handles`) into multiple
 * `buildSemApi()` constructions, exactly as `tool.ts`'s `registerSemCode`
 * does (one `handles` instance per pi session, threaded into every
 * `execute()` call) to get session-wide resolution.
 */
test("find()'s hits each carry a unique h<n> handle", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.find("add")) as { hits: Array<{ name: string; h: string }> };
    assert.equal(result.hits.length, 1);
    assert.match(result.hits[0]!.h, /^h\d+$/, `expected an h<n> handle, got: ${JSON.stringify(result.hits[0])}`);
  });
});

test("callers()'s rows each carry a unique h<n> handle, distinct from find()'s in the same run", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const found = (await api.find("add")) as { hits: Array<{ h: string }> };
    const called = (await api.callers("add")) as { callers: Array<{ name: string; h: string }> };
    assert.equal(called.callers.length, 2);
    const allHandles = [found.hits[0]!.h, ...called.callers.map((c) => c.h)];
    assert.equal(new Set(allHandles).size, allHandles.length, `expected all handles distinct across calls in one run, got: ${JSON.stringify(allHandles)}`);
  });
});

test("read() accepts a handle from an earlier find() result and reads the SAME entity", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const found = (await api.find("twice")) as { hits: Array<{ name: string; h: string }> };
    const handle = found.hits[0]!.h;

    const byHandle = (await api.read(handle)) as { entity: { name: string }; content: string };
    const byLocator = (await api.read({ name: "twice", file: "calls.ts" })) as { entity: { name: string }; content: string };

    assert.equal(byHandle.entity.name, "twice");
    assert.equal(byHandle.content, byLocator.content, "reading via handle must resolve to the exact same entity as reading via the equivalent locator");
  });
});

test("read() accepts a handle from an earlier callers() result", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const called = (await api.callers("add")) as { callers: Array<{ name: string; h: string }> };
    const someCaller = called.callers[0]!;
    const byHandle = (await api.read(someCaller.h)) as { entity: { name: string } };
    assert.equal(byHandle.entity.name, someCaller.name);
  });
});

test("read() accepts an array mixing handles and plain locators", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const found = (await api.find("twice")) as { hits: Array<{ h: string }> };
    // { full: true }: this test is about handle+locator resolution, not
    // v2 item 5's headers-by-default-for-2+ behavior (covered separately
    // in api-read.test.ts) -- opt back into full bodies to keep asserting
    // on `.entity.name`.
    const results = (await api.read([found.hits[0]!.h, { name: "thrice", file: "calls.ts" }], { full: true })) as Array<{ entity: { name: string } }>;
    assert.equal(results.length, 2);
    assert.deepEqual(
      results.map((r) => r.entity.name).sort(),
      ["thrice", "twice"],
    );
  });
});

test("read() with an unknown/expired handle throws a clear error, not a silent misresolution", async () => {
  const api = buildSemApi({ cwd: process.cwd(), semBin: "sem" });
  await assert.rejects(() => api.read("h999"), /not a known handle|unknown handle/i);
});

test("handles are isolated by DEFAULT across separate buildSemApi() constructions that don't share a HandleStore", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const apiA = buildSemApi({ cwd: dir, semBin: "sem" });
    const found = (await apiA.find("add")) as { hits: Array<{ h: string }> };

    const apiB = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => apiB.read(found.hits[0]!.h), /not a known handle|unknown handle/i);
  });
});

test("handles ARE session-scoped when the SAME HandleStore is threaded into separate buildSemApi() calls, as tool.ts does per session", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const handles = createHandleStore();

    // Call 1: mirrors one sem_code tool invocation.
    const apiCall1 = buildSemApi({ cwd: dir, semBin: "sem", handles });
    const found = (await apiCall1.find("add")) as { hits: Array<{ name: string; h: string }> };
    const handle = found.hits[0]!.h;

    // Call 2: a SEPARATE buildSemApi() construction (a later sem_code call
    // in the same pi session) sharing the SAME handles instance -- the
    // handle from call 1 must resolve here.
    const apiCall2 = buildSemApi({ cwd: dir, semBin: "sem", handles });
    const byHandle = (await apiCall2.read(handle)) as { entity: { name: string } };
    assert.equal(byHandle.entity.name, "add", "a handle minted in an earlier sem_code call must resolve in a later one within the same session");
  });
});
