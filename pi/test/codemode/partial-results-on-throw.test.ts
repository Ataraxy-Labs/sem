import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerSemCode } from "../../src/codemode/tool.ts";
import { runInSandbox } from "../../src/codemode/sandbox.ts";
import { buildSemApi } from "../../src/codemode/api.ts";

/**
 * P6 of the 2026-09-02 transcript study. The addendum's own second line
 * ("one script per turn, not many single-verb round trips") pushes agents
 * toward large scripts -- 77% of calls combine 2+ verbs -- and every verb
 * throws on a soft failure, so ONE bad guess in step 3 discarded steps 1
 * and 2 as well. 206 scripts died this way in 147 of 327 runs (45%), taking
 * ~518 already-planned sem.* calls with them, and only 5.4% of scripts used
 * try/catch.
 *
 * The exact shape, from astropy__astropy-13398:
 *
 *   const g = await sem.grep([...]);       // succeeded
 *   const o = await sem.outline("...");    // succeeded
 *   const reads = await sem.read([...]);   // threw
 *   console.log(JSON.stringify({g,o,reads}, null, 2));
 *
 *   -> sem_code: Error: sem.read: no entity named "cirs_to_altaz" found ...
 *
 * Two successful searches, zero information returned. This file pins that
 * the completed calls' own results, and anything logged before the throw,
 * come back ALONGSIDE the error -- the tax scaled with how well the agent
 * followed the tool's own advice, which is exactly backwards.
 */

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "..", "tools", "fixtures");

type ToolExecute = (
  toolCallId: string,
  params: { code: string; timeout_ms?: number },
  signal: undefined,
  onUpdate: undefined,
  ctx: { cwd: string },
) => Promise<{ content: Array<{ type: string; text: string }>; details: Record<string, unknown> }>;

function semCodeExecute(): ToolExecute {
  let execute: ToolExecute | undefined;
  const pi = {
    registerTool(def: { name: string; execute?: ToolExecute }) {
      if (def.name === "sem_code") execute = def.execute;
    },
    on() {},
  } as unknown as ExtensionAPI;
  registerSemCode(pi);
  assert.ok(execute, "sem_code must register an execute()");
  return execute;
}

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-partial-results-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("P6: the sandbox retains every COMPLETED call's own result, not just whether it was ok", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await runInSandbox(
      `const o = await sem.outline("math.ts");
       await sem.read({ name: "no_such_entity", file: "math.ts" });
       return o;`,
      { sem: api },
    );

    assert.equal(result.ok, false, "the third call must still fail -- this is not about swallowing errors");
    assert.equal(result.completed.length, 1, "the outline() that succeeded must be retained");
    assert.equal(result.completed[0]?.fn, "outline");
    const outline = result.completed[0]?.value as { file: string; entities: unknown[] };
    assert.equal(outline.file, "math.ts");
    assert.ok(outline.entities.length > 0, "the retained value is the real result, not a placeholder");
  });
});

test("P6: the astropy-13398 shape -- two searches then a failing read returns BOTH searches with the error", async () => {
  await withTempCopy(["math.ts", "calculator.ts"], async (dir) => {
    const execute = semCodeExecute();
    await assert.rejects(
      () =>
        execute(
          "t1",
          {
            code: `const g = await sem.grep("add", { literal: true });
                   const o = await sem.outline("calculator.ts");
                   const reads = await sem.read({ name: "cirs_to_altaz", file: "calculator.ts" });
                   return { g, o, reads };`,
          },
          undefined,
          undefined,
          { cwd: dir },
        ),
      (err: Error) => {
        assert.match(err.message, /no entity named "cirs_to_altaz"/, "the real error must still be the headline");
        assert.match(err.message, /2 sem\.\* call\(s\) completed/, "the surviving work must be counted");
        assert.match(err.message, /"grep"/, "the grep that succeeded must come back");
        assert.match(err.message, /"outline"/, "the outline that succeeded must come back");
        assert.match(err.message, /calculator\.ts/, "with its real content, not just its name");
        return true;
      },
    );
  });
});

test("P6: anything logged before the throw survives too", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const execute = semCodeExecute();
    await assert.rejects(
      () =>
        execute(
          "t2",
          {
            code: `sem.log("located the entity, editing next");
                   await sem.outline("math.ts");
                   throw new Error("boom");`,
          },
          undefined,
          undefined,
          { cwd: dir },
        ),
      (err: Error) => {
        assert.match(err.message, /boom/);
        assert.match(err.message, /located the entity, editing next/, "console.log/sem.log output before the throw must not be discarded");
        return true;
      },
    );
  });
});

test("P6: a script that throws before ANY sem.* call still reports just the error, with no empty partial-results noise", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const execute = semCodeExecute();
    await assert.rejects(
      () => execute("t3", { code: `throw new Error("nothing ran");` }, undefined, undefined, { cwd: dir }),
      (err: Error) => {
        assert.match(err.message, /nothing ran/);
        assert.doesNotMatch(err.message, /call\(s\) completed/);
        assert.doesNotMatch(err.message, /partial results/);
        return true;
      },
    );
  });
});

test("P6: a SUCCESSFUL run is unchanged -- partial results are an error-path affordance only", async () => {
  await withTempCopy(["math.ts"], async (dir) => {
    const execute = semCodeExecute();
    const result = await execute("t4", { code: `const o = await sem.outline("math.ts"); return o.entity_count;` }, undefined, undefined, {
      cwd: dir,
    });
    const text = result.content[0]?.text ?? "";
    assert.doesNotMatch(text, /call\(s\) completed/);
    assert.doesNotMatch(text, /partial results/);
  });
});
