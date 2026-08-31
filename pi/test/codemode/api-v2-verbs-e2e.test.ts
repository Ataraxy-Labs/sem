import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-v2-verbs-e2e-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * v2 item 1 (question verbs): a single real sandboxed script exercising
 * blast/why/where/explain/changed end to end -- proving they work through
 * the ACTUAL runInSandbox trampoline (context-native JSON round-trip,
 * handle strings passing through opaquely), not just at the api.ts layer
 * every other test in this file hits directly.
 */
test("e2e: blast/why/where/explain/changed all work through a real sandboxed run", async () => {
  await withTempCopy(["chain.ts", "chain-spec.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const script = `
      const b = await sem.blast({ name: "gamma" }, { depth: 2 });
      const w = await sem.why({ name: "alpha" }, { name: "delta" });
      const wh = await sem.where("alpha");
      const ex = await sem.explain({ name: "alpha", file: "chain.ts" });
      const before = await sem.changed();
      await sem.edit({ file: "chain.ts", entity: { name: "delta" }, op: "replace", content: "export function delta(): number {\\n  return 2;\\n}" });
      const after = await sem.changed();
      return { b, w, wh, ex, before, after };
    `;
    const result = await runInSandbox(script, { sem: api });
    assert.equal(result.ok, true, JSON.stringify(result));

    const value = result.value as {
      b: { entity: string; rows: Array<{ name: string; hops: number; reason: string; h: string }> };
      w: { connected: boolean; summary: string };
      wh: { concept: string; rows: Array<{ kind: string }> };
      ex: { name: string; paragraph: string };
      before: { count: number };
      after: { count: number; files: string[] };
    };

    assert.equal(value.b.entity, "gamma");
    assert.ok(value.b.rows.some((r) => r.name === "beta" && r.hops === 1 && r.reason === "caller"));

    assert.equal(value.w.connected, true);
    assert.equal(value.w.summary, "alpha -> beta -> gamma -> delta (3 hops)");

    assert.equal(value.wh.concept, "alpha");
    assert.ok(value.wh.rows.some((r) => r.kind === "definition"));

    assert.equal(value.ex.name, "alpha");
    assert.match(value.ex.paragraph, /alpha/);

    assert.equal(value.before.count, 0, "changed() before any edit should be empty");
    assert.equal(value.after.count, 1, "changed() after one edit should show it");
    assert.deepEqual(value.after.files, ["chain.ts"]);
  });
});
