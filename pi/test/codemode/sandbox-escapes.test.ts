import { test } from "node:test";
import assert from "node:assert/strict";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

/**
 * The sandbox contract (src/codemode/DESIGN.md): the script's only globals
 * are `sem` and `console` (console.log captured). No require/import/
 * process/fetch, and no globalThis path back to the host realm — including
 * the well-documented node:vm caveat that a HOST-REALM function object
 * exposed into the context (any `sem.*` bridge) carries a `.constructor`
 * chain back to the host's real `Function` intrinsic. This file is the
 * arbiter for that caveat: it plants a marker on the real process.env and
 * asserts no script, however it tries, can read it back.
 */

const fakeSem = { ping: async () => "pong", log: () => {} };

test("bare `require(...)` throws inside the sandbox, does not resolve a module", async () => {
  const result = await runInSandbox(`return require("node:child_process");`, { sem: fakeSem });
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /require is not defined|require/i);
});

test("bare `process.*` throws inside the sandbox, does not reach the host process", async () => {
  const result = await runInSandbox(`return process.pid;`, { sem: fakeSem });
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /process is not defined|process/i);
});

test("bare `fetch(...)` throws inside the sandbox, no network reachable", async () => {
  const result = await runInSandbox(`return fetch("http://127.0.0.1:1");`, { sem: fakeSem });
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /fetch is not defined|fetch/i);
});

test("dynamic `import(...)` is refused, not silently resolved", async () => {
  const result = await runInSandbox(`return await import("node:fs");`, { sem: fakeSem });
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /import|dynamic/i);
});

test("globalThis inside the sandbox exposes only sem and console -- no host globals leak in", async () => {
  const result = await runInSandbox(`return Object.keys(globalThis).sort();`, { sem: fakeSem });
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.deepEqual(result.value, ["console", "sem"]);
});

test("globalThis.process / globalThis.require are undefined, not just the bare identifiers", async () => {
  const result = await runInSandbox(
    `return { p: typeof globalThis.process, r: typeof globalThis.require, g: typeof globalThis.global };`,
    { sem: fakeSem },
  );
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.deepEqual(result.value, { p: "undefined", r: "undefined", g: "undefined" });
});

test("VM-native constructor chains (this / [] / async-literal) cannot read a real process.env marker", async () => {
  const marker = `sem-code-escape-${Math.random().toString(36).slice(2)}`;
  const previous = process.env.__SEM_CODE_ESCAPE_TEST__;
  process.env.__SEM_CODE_ESCAPE_TEST__ = marker;
  try {
    const result = await runInSandbox(
      `
      const attempts = [];
      try { attempts.push((this.constructor.constructor("return process.env.__SEM_CODE_ESCAPE_TEST__"))()); } catch (e) { attempts.push("threw: " + e.message); }
      try { attempts.push(((async () => {}).constructor("return process.env.__SEM_CODE_ESCAPE_TEST__"))()); } catch (e) { attempts.push("threw: " + e.message); }
      try { attempts.push(([]).constructor.constructor("return process.env.__SEM_CODE_ESCAPE_TEST__")()); } catch (e) { attempts.push("threw: " + e.message); }
      return attempts;
      `,
      { sem: fakeSem },
    );
    const serialized = JSON.stringify(result);
    assert.ok(!serialized.includes(marker), `escape leaked the marker: ${serialized}`);
  } finally {
    if (previous === undefined) delete process.env.__SEM_CODE_ESCAPE_TEST__;
    else process.env.__SEM_CODE_ESCAPE_TEST__ = previous;
  }
});

// THE hard case: `sem.ping` is a HOST-REALM function object (it comes from
// `fakeSem`, defined in this test file, outside the vm entirely) before
// sandbox.ts gets to it. A naive `vm.createContext({sem: fakeSem})` would
// expose `sem.ping.constructor` as the HOST's real `Function` intrinsic --
// proven empirically (a throwaway probe) to leak straight through even
// with `codeGeneration:{strings:false}` set, because that restriction is
// checked against the intrinsic's OWN unrestricted home context, not the
// sandbox's. This test is the actual arbiter for sandbox.ts's mitigation
// (compiling a context-native trampoline instead of exposing host
// functions directly) -- it must stay green under whatever implementation
// approach is used.
test("the constructor-chain escape through an EXPOSED sem.* function (the real cross-realm vector) cannot read a marker", async () => {
  const marker = `sem-code-escape-${Math.random().toString(36).slice(2)}`;
  const previous = process.env.__SEM_CODE_ESCAPE_TEST__;
  process.env.__SEM_CODE_ESCAPE_TEST__ = marker;
  try {
    const result = await runInSandbox(
      `
      const attempts = [];
      // via the function object itself, before awaiting its call
      try {
        const p = sem.ping();
        attempts.push((p.constructor.constructor("return process.env.__SEM_CODE_ESCAPE_TEST__"))());
      } catch (e) { attempts.push("threw: " + e.message); }
      // via the resolved value
      try {
        const resolved = await sem.ping();
        attempts.push((resolved.constructor.constructor("return process.env.__SEM_CODE_ESCAPE_TEST__"))());
      } catch (e) { attempts.push("threw: " + e.message); }
      // directly off sem.ping itself
      try {
        attempts.push((sem.ping.constructor.constructor("return process.env.__SEM_CODE_ESCAPE_TEST__"))());
      } catch (e) { attempts.push("threw: " + e.message); }
      return attempts;
      `,
      { sem: fakeSem },
    );
    const serialized = JSON.stringify(result);
    assert.ok(!serialized.includes(marker), `escape leaked the marker through an exposed sem.* function: ${serialized}`);
  } finally {
    if (previous === undefined) delete process.env.__SEM_CODE_ESCAPE_TEST__;
    else process.env.__SEM_CODE_ESCAPE_TEST__ = previous;
  }
});

test("the sem object handed to the sandbox is frozen -- a script cannot add or replace an entry point", async () => {
  const result = await runInSandbox(
    `
    "use strict";
    let threw = false;
    try { sem.ping = async () => "hijacked"; } catch (e) { threw = true; }
    return { threw, stillOriginal: (await sem.ping()) === "pong" };
    `,
    { sem: fakeSem },
  );
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.deepEqual(result.value, { threw: true, stillOriginal: true });
});
