import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemGraph, type SemGraphParams } from "../../src/tools/sem-graph.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
// Reuses the codemode lane's chain.ts/loop.ts/dup-a.ts/dup-b.ts fixtures
// (test/codemode/fixtures/) rather than duplicating them — sem_graph's
// performSemGraph and code mode's sem.graph() both bottom out in the exact
// same internal/graph.ts#computeGraph, so the same fixtures exercise both.
const FIXTURES = join(__dirname, "..", "codemode", "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "sem-graph-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function run(params: SemGraphParams, cwd: string) {
  return performSemGraph(params, { cwd, semBin: "sem" });
}

// chain.ts: alpha -> beta -> gamma -> delta (no cycle)
// loop.ts: loopA -> loopB -> loopC -> loopA (a 3-cycle, isolated from chain.ts)

test("sem_graph direction=out from beta finds gamma (what beta calls) but not alpha (who calls beta)", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ seed: "beta", direction: "out", hops: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /gamma \(chain\.ts\)/);
    assert.doesNotMatch(outcome.text, /alpha/);
    const details = outcome.details as { node_count?: number; edge_count?: number; truncated?: boolean };
    assert.equal(details.truncated, false);
    assert.ok((details.node_count ?? 0) >= 2);
  });
});

test("sem_graph direction=in from beta finds alpha (who calls beta) but not gamma (what beta calls)", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ seed: "beta", direction: "in", hops: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /alpha \(chain\.ts\)/);
    assert.doesNotMatch(outcome.text, /gamma/);
  });
});

test("sem_graph header reports seed, hops, direction, and node/edge counts", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ seed: "alpha", direction: "both", hops: 3 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const lines = outcome.text.split("\n");
    assert.match(lines[0] ?? "", /^sem_graph: alpha \[hops=3, direction=both\] — \d+ nodes, \d+ edges/);
  });
});

test("sem_graph groups edges under hop headers", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ seed: "alpha", direction: "out", hops: 3 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /hop 1:/);
    assert.match(outcome.text, /hop 2:/);
  });
});

test("seeds= accepts multiple seeds in one call, combining their neighborhoods", async () => {
  await withTempCopy(["chain.ts", "loop.ts"], async (dir) => {
    const outcome = await run({ seeds: ["alpha", "loopA"], hops: 1 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    assert.match(outcome.text, /alpha/);
    assert.match(outcome.text, /loopA/);
  });
});

test("sem_graph over a cycle (loop.ts) terminates and reports truncated: false", async () => {
  await withTempCopy(["loop.ts"], async (dir) => {
    const outcome = await run({ seed: "loopA", hops: 5 }, dir);
    assert.equal(outcome.isError, false, outcome.text);
    const details = outcome.details as { truncated?: boolean; node_count?: number };
    assert.equal(details.truncated, false);
    assert.equal(details.node_count, 3);
  });
});

test("sem_graph refuses with candidate files when the seed name is ambiguous, and file= disambiguates it", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts"], async (dir) => {
    const ambiguous = await run({ seed: "widget" }, dir);
    assert.equal(ambiguous.isError, true);
    assert.match(ambiguous.text, /ambiguous/i);

    const disambiguated = await run({ seed: "widget", file: "dup-a.ts" }, dir);
    assert.equal(disambiguated.isError, false, disambiguated.text);
  });
});

test("sem_graph reports a not-found seed as a refusal, not a silent empty result", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({ seed: "totallyMissingName" }, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /no entity named/i);
  });
});

test("pass either seed= or seeds= — neither is a plain, explicit error", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const outcome = await run({}, dir);
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /pass either seed=/);
  });
});
