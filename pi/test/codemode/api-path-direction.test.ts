import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

/**
 * path()/why() direction semantics — see fixtures/chain-direction.ts for the
 * shape: the real directed chain source -> m1 -> m2 -> target coexists with a
 * hub calling BOTH endpoints, so the undirected walk finds a SHORTER, spurious
 * source-hub-target "connection" that is not a call chain. The old undirected
 * default returned exactly that wrong answer on a real repo (caught by the
 * oracle-floor work, which re-derived the golden chain independently);
 * directed "out" is now the default and "any" the explicit opt-in.
 */

function withFixture<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "path-direction-"));
  cpSync(join(FIXTURES, "chain-direction.ts"), join(dir, "chain-direction.ts"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

const names = (chain: Array<{ name: string }> | null): string[] => (chain ?? []).map((n) => n.name);

test("default (out) returns the REAL directed chain, not the shorter undirected shortcut", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const chain = (await api.path("source", "target")) as Array<{ name: string }> | null;
    assert.deepEqual(names(chain), ["source", "m1", "m2", "target"]);
  });
});

test("direction:'any' reproduces the old undirected behavior — the 2-hop spurious connection through hub", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const chain = (await api.path("source", "target", { direction: "any" })) as Array<{ name: string }> | null;
    assert.deepEqual(names(chain), ["source", "hub", "target"], "the opt-in must preserve the old shape exactly");
  });
});

test("default (out) from target to source is null — no directed path exists backward", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const chain = (await api.path("target", "source")) as Array<{ name: string }> | null;
    assert.equal(chain, null, "target has no out-edges reaching source; a directed default must say so, not invent a connection");
  });
});

test("direction:'in' walks edges backward — target to source through the reversed chain", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const chain = (await api.path("target", "source", { direction: "in" })) as Array<{ name: string }> | null;
    assert.deepEqual(names(chain), ["target", "m2", "m1", "source"]);
  });
});

test("max_hops still binds under the directed default", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const chain = (await api.path("source", "target", { max_hops: 2 })) as Array<{ name: string }> | null;
    assert.equal(chain, null, "the real chain needs 3 hops; max_hops 2 must return null rather than fall back to the spurious undirected 2-hop route");
  });
});

test("why() default summary reads as the real call chain", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.why("source", "target")) as { connected: boolean; hops: number; summary: string };
    assert.equal(result.connected, true);
    assert.equal(result.hops, 3);
    assert.equal(result.summary, "source -> m1 -> m2 -> target (3 hops)");
  });
});

test("why() with no directed connection says so instead of inventing one", async () => {
  await withFixture(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.why("target", "source")) as { connected: boolean; summary: string };
    assert.equal(result.connected, false);
    assert.match(result.summary, /no connection found/);
  });
});
