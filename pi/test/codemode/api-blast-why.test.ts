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
  const dir = mkdtempSync(join(tmpdir(), "codemode-blast-why-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

// chain.ts: alpha -> beta -> gamma -> delta (no cycle).
// chain-spec.ts: a test() calling alpha, giving blast() a real entityType
// "test" edge to find (verified empirically, not assumed).

/**
 * v2 item 1 (question verbs): blast() answers "who's affected if I change
 * this" in ONE call -- callers ∪ transitive dependents ∪ affected tests,
 * deduped, with a per-row hop count -- replacing a model composing
 * sem.callers()+sem.impact() itself.
 */
test("blast(): direct callers come back at hops:1 with reason 'caller'", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.blast({ name: "gamma" }, { depth: 2 })) as {
      entity: string;
      depth: number;
      truncated: boolean;
      rows: Array<{ name: string; reason: string; hops: number; h: string }>;
    };
    assert.equal(result.entity, "gamma");
    const beta = result.rows.find((r) => r.name === "beta");
    assert.ok(beta, `expected beta (gamma's direct caller) in ${JSON.stringify(result.rows)}`);
    assert.equal(beta!.reason, "caller");
    assert.equal(beta!.hops, 1);
    assert.match(beta!.h, /^h\d+$/);
  });
});

test("blast(): transitive callers beyond hop 1 come back with reason 'dependent'", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.blast({ name: "gamma" }, { depth: 2 })) as { rows: Array<{ name: string; reason: string; hops: number }> };
    const alpha = result.rows.find((r) => r.name === "alpha");
    assert.ok(alpha, `expected alpha (gamma's transitive caller, via beta) in ${JSON.stringify(result.rows)}`);
    assert.equal(alpha!.reason, "dependent");
    assert.equal(alpha!.hops, 2);
  });
});

test("blast(): does not include the seed itself, or what the seed CALLS (only what's affected BY it)", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.blast({ name: "gamma" }, { depth: 2 })) as { rows: Array<{ name: string }> };
    const names = result.rows.map((r) => r.name);
    assert.ok(!names.includes("gamma"), `seed should not appear in its own blast rows: ${JSON.stringify(names)}`);
    assert.ok(!names.includes("delta"), `gamma's callee should not appear -- blast is who's affected BY gamma, not what gamma depends on: ${JSON.stringify(names)}`);
  });
});

test("blast(): respects depth -- alpha (2 hops from gamma) is excluded when depth:1", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.blast({ name: "gamma" }, { depth: 1 })) as { rows: Array<{ name: string }> };
    const names = result.rows.map((r) => r.name);
    assert.ok(names.includes("beta"));
    assert.ok(!names.includes("alpha"), `depth:1 should not reach alpha (2 hops away): ${JSON.stringify(names)}`);
  });
});

test("blast(): a test() call site exercising the seed comes back with reason 'test'", async () => {
  await withTempCopy(["chain.ts", "chain-spec.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.blast({ name: "alpha" }, { depth: 1 })) as { rows: Array<{ name: string; reason: string; type: string }> };
    const testRow = result.rows.find((r) => r.reason === "test");
    assert.ok(testRow, `expected a 'test' row in ${JSON.stringify(result.rows)}`);
    assert.equal(testRow!.type, "test");
  });
});

test("blast(): throws with candidates when the seed name is ambiguous, same as graph()", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.blast({ name: "widget" }), /ambiguous/i);
  });
});

/**
 * v2 item 1: why() answers "how are A and B connected" as a single
 * compact answer -- the chain plus a one-line summary -- instead of a
 * model composing its own callers/impact walk.
 */
test("why(): finds the exact chain between two connected entities, with a one-line summary", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.why({ name: "alpha" }, { name: "delta" })) as {
      connected: boolean;
      hops: number;
      chain: Array<{ name: string }>;
      summary: string;
    };
    assert.equal(result.connected, true);
    assert.equal(result.hops, 3);
    assert.deepEqual(
      result.chain.map((n) => n.name),
      ["alpha", "beta", "gamma", "delta"],
    );
    assert.equal(result.summary, "alpha -> beta -> gamma -> delta (3 hops)");
  });
});

test("why(): reports connected:false with a clear summary when there is no path", async () => {
  await withTempCopy(["chain.ts", "loop.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.why({ name: "alpha" }, { name: "loopA" })) as { connected: boolean; hops: number; chain: unknown[]; summary: string };
    assert.equal(result.connected, false);
    assert.equal(result.hops, 0);
    assert.deepEqual(result.chain, []);
    assert.match(result.summary, /no connection/i);
  });
});
