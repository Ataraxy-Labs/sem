import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * Pure codespace is code mode's DEFAULT: with PI_SEM_PURE UNSET, the
 * codespace is the WHOLE surface. One tool ([sem_code]), no active
 * builtins at all -- ask (blast/why/where/explain), act (edit/rename/add),
 * verify (check), report the entity diff. This file pins the three facts
 * that make that livable, deliberately with the env var ABSENT so it is
 * the default being proven, not an opt-in:
 *   1. the active tool set is exactly ["sem_code"],
 *   2. sem.write is refused with a pointer at the one creation door,
 *   3. sem.check works with NO bash builtin (it shells its command
 *      directly, never through a tool).
 * The explicit PI_SEM_PURE=0 opt-out (bash/write builtins restored) is
 * proven by test/bridge/extension-code-mode-fail-closed.test.ts, which
 * sets it at module load and pins [bash, write, sem_code] throughout.
 *
 * PI_SEM_PURE and PI_SEM_MODE are read at module load by
 * extensions/pi-sem.ts, so they are arranged here BEFORE the dynamic
 * import -- a static import would hoist past this. PI_SEM_MODE=code is
 * still required: pure is code mode's default, not a mode of its own.
 */
delete process.env.PI_SEM_PURE;
process.env.PI_SEM_MODE = "code";
const { startServersAndRegisterTools } = await import("../../extensions/pi-sem.ts");
const { buildSemApi } = await import("../../src/codemode/api.ts");
const { registerSemCode } = await import("../../src/codemode/tool.ts");

function makeFakePi() {
  const registeredToolNames: string[] = [];
  const activeToolsCalls: string[][] = [];
  const pi = {
    registerTool: (def: { name: string }) => {
      registeredToolNames.push(def.name);
    },
    registerCommand: () => {},
    on: () => {},
    setActiveTools: (names: string[]) => {
      activeToolsCalls.push(names);
    },
    getActiveTools: () => activeToolsCalls.at(-1) ?? [],
    getAllTools: () => registeredToolNames.map((name) => ({ name })),
  };
  return { pi: pi as unknown as ExtensionAPI, registeredToolNames, activeToolsCalls };
}

const ctx = {
  ui: { notify: () => {} },
  model: undefined,
  cwd: process.cwd(),
} as unknown as ExtensionContext;

test("pure mode: the active tool set is exactly [sem_code] -- no builtins, nothing else", async () => {
  const { pi, activeToolsCalls, registeredToolNames } = makeFakePi();
  const outcome = await startServersAndRegisterTools(pi, ctx, {
    servers: [],
    // Deliberately asks for builtins -- pure mode must override, not merge.
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  });
  try {
    assert.equal(activeToolsCalls.length, 1);
    assert.deepEqual(activeToolsCalls[0], ["sem_code"], "pure mode's whole surface is the one tool");
    assert.ok(registeredToolNames.includes("sem_code"));
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("pure mode: sem.write is refused with a pointer at sem.add, and touches nothing", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pure-write-"));
  try {
    // Direct library callers opt in via deps.pure (the env default is the
    // HOST's to resolve -- see the registerSemCode default-proof below).
    const sem = buildSemApi({ cwd: dir, semBin: "sem", pure: true });
    await assert.rejects(sem.write("new.rs", "pub fn x() {}\n"), (err: Error) => {
      assert.match(err.message, /disabled in pure mode/);
      assert.match(err.message, /sem\.add/);
      return true;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pure is the DEFAULT: registerSemCode with PI_SEM_PURE unset refuses sem.write end to end", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pure-default-"));
  try {
    // Guard the premise this test exists to prove: the env var is absent.
    assert.equal(process.env.PI_SEM_PURE, undefined);
    const defs: { name: string; execute?: (...args: unknown[]) => Promise<{ details: Record<string, unknown> }> }[] = [];
    const pi = {
      registerTool: (def: (typeof defs)[number]) => defs.push(def),
      setActiveTools: () => {},
      on: () => {},
    } as unknown as ExtensionAPI;
    registerSemCode(pi);
    const execute = defs.find((d) => d.name === "sem_code")!.execute!;
    const result = await execute(
      "call-1",
      { code: `try { await sem.write("x.txt", "y"); return "wrote"; } catch (e) { return "refused: " + e.message; }` },
      undefined,
      undefined,
      { cwd: dir },
    );
    const value = (result.details as { value?: unknown }).value;
    assert.match(String(value), /disabled in pure mode/);
    assert.match(String(value), /sem\.add/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pure mode: sem.check needs no bash builtin -- a cmd override (within the allowlist) runs and reports directly", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pure-check-"));
  // The cmd allowlist (src/codemode/api.ts) only runs detected project
  // runners or explicit PI_SEM_CHECK_ALLOW entries -- this dir has no
  // manifest, so extend the allowlist with the interpreter under test
  // rather than proving anything about arbitrary command execution, which
  // is exactly what the allowlist exists to prevent.
  const prevAllow = process.env.PI_SEM_CHECK_ALLOW;
  process.env.PI_SEM_CHECK_ALLOW = process.execPath;
  try {
    writeFileSync(join(dir, "ok.txt"), "x\n");
    const sem = buildSemApi({ cwd: dir, semBin: "sem" });
    const green = (await sem.check({ cmd: `${process.execPath} -e "process.exit(0)"` })) as { pass: boolean };
    assert.equal(green.pass, true);
    const red = (await sem.check({ cmd: `${process.execPath} -e "console.error('boom'); process.exit(3)"` })) as { pass: boolean };
    assert.equal(red.pass, false);
  } finally {
    if (prevAllow === undefined) delete process.env.PI_SEM_CHECK_ALLOW;
    else process.env.PI_SEM_CHECK_ALLOW = prevAllow;
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pure mode: sem.check refuses an arbitrary cmd not on the allowlist -- pure mode has no general-purpose shell, check() included", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pure-check-refuse-"));
  try {
    const sem = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(sem.check({ cmd: `${process.execPath} -e "process.exit(0)"` }), (err: Error) => {
      assert.match(err.message, /^sem\.check:/);
      assert.match(err.message, /PI_SEM_CHECK_ALLOW/);
      return true;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
