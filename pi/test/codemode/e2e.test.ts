import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "..", "tools", "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-e2e-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

// The whole point of code mode: one sem_code call does what took several
// tiny tool calls before (89a78c7 eval: 30 sem_grep calls -> 56 turns for a
// multi-file task). This script greps 3 patterns in ONE sem.grep call, then
// reads the 2 entities that matched, all inside a single sandboxed run.
test("a script that greps 3 patterns then reads the 2 matching entities completes in one sandboxed run", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const script = `
      const grepped = await sem.grep(["export function add", "export function sub", "export function calculate"]);
      const files = new Set();
      for (const r of grepped.results) for (const hit of r.hits) files.add(hit.file);

      const add = await sem.read({ name: "add", file: "math.ts" });
      const calc = await sem.read({ name: "calculate", file: "calculator.ts" });

      const lines = [
        "files: " + [...files].sort().join(","),
        ...add.content.trim().split("\\n"),
        ...calc.content.trim().split("\\n"),
      ];
      return lines.slice(0, 10).join("\\n");
    `;

    const result = await runInSandbox(script, { sem: api });

    assert.equal(result.ok, true, JSON.stringify(result));
    assert.equal(typeof result.value, "string");
    const value = result.value as string;
    assert.match(value, /math\.ts/);
    assert.match(value, /calculator\.ts/);
    assert.match(value, /function add/);
    assert.match(value, /function calculate/);

    // One sem.grep call (batched across 3 patterns) + 2 sem.read calls -- not
    // 3 separate grep round-trips. This is the turn-count win the design exists for.
    assert.equal(result.callCount, 3, `expected exactly 3 sem.* calls, got ${result.callCount}`);
  });
});
