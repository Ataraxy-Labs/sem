import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemPath, type SemPathParams } from "../../src/tools/sem-path.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
// Reuses the codemode lane's chain.ts/loop.ts/dup-a.ts/dup-b.ts fixtures —
// sem_path's performSemPath and code mode's sem.path() both bottom out in
// the exact same internal/graph.ts#computePath.
const FIXTURES = join(__dirname, "..", "codemode", "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-path-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemPathParams, cwd: string) {
  return performSemPath(params, { cwd, semBin: "sem" });
}

// chain.ts: alpha -> beta -> gamma -> delta (no cycle, deterministic shortest path)
// loop.ts: loopA -> loopB -> loopC -> loopA (a 3-cycle, isolated from chain.ts)

test("sem_path finds the exact ordered chain from alpha to delta as one A -> B -> C line", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ a: "alpha", b: "delta" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /^sem_path: alpha \(chain\.ts:\d+\) -> beta \(chain\.ts:\d+\) -> gamma \(chain\.ts:\d+\) -> delta \(chain\.ts:\d+\)$/);
    const details = outcome.details as { found?: boolean; hops?: number };
    assert.equal(details.found, true);
    assert.equal(details.hops, 3);
  });
});

test("sem_path over a cycle terminates and finds a real path rather than hanging", async () => {
  await withTempCopy(["loop.ts"], async (dir) => {
    const outcome = await run({ a: "loopA", b: "loopC" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /^sem_path: loopA .* -> .*loopC/);
  });
});

test("sem_path reports 'no path within N hops' as a plain success, not an error, when genuinely unconnected", async () => {
  await withTempCopy(["chain.ts", "loop.ts"], async (dir) => {
    const outcome = await run({ a: "alpha", b: "loopA" }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /no connection found between "alpha" and "loopA" within 6 hops/);
    const details = outcome.details as { found?: boolean; chain?: unknown };
    assert.equal(details.found, false);
    assert.equal(details.chain, null);
  });
});

test("max_hops bounds the search, reporting no path when the real one is longer than the bound", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ a: "alpha", b: "delta", max_hops: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /no connection found.*within 1 hops/);
  });
});

test("a_file=/b_file= disambiguate an ambiguous name", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts", "chain.ts"], async (dir) => {
    const ambiguous = await run({ a: "widget", b: "alpha" }, dir);
    assert.equal(ambiguous.isError, true);
    assert.match(ambiguous.text, /ambiguous/i);

    const disambiguated = await run({ a: "widget", a_file: "dup-a.ts", b: "alpha" }, dir);
    assert.equal(disambiguated.isError, false, disambiguated.text);
  });
});

test("sem_path refuses cleanly when either entity doesn't exist", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ a: "alpha", b: "totallyMissingName" }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named/i);
  });
});
