import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { runInSandbox, createRunCancellation } from "../../src/codemode/sandbox.ts";
import { buildSemApi } from "../../src/codemode/api.ts";

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-sandbox-limits-test-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * The four runaway-loop guards DESIGN.md commits to: wall-clock timeout
 * covering BOTH a synchronous CPU-bound loop (vm.Script's own `timeout`
 * catches this) and an async hang past an `await` (which vm's `timeout`
 * does NOT cover -- proven here by using a promise that never settles, so
 * this test only passes if sandbox.ts also races an outer timer), an
 * honest output cap, and a call-count cap on sem.* calls.
 */

const fakeSem = { log: () => {} };

test("a synchronous infinite loop is killed by the wall-clock timeout", async () => {
  const started = Date.now();
  const result = await runInSandbox(`while (true) {}`, { sem: fakeSem, timeoutMs: 300 });
  const elapsed = Date.now() - started;
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /timeout|timed out/i);
  assert.ok(elapsed < 5_000, `took ${elapsed}ms to kill a 300ms-budget sync loop`);
});

test("an async hang (an awaited promise that never settles) is also killed by the wall-clock timeout", async () => {
  const started = Date.now();
  const result = await runInSandbox(`await new Promise(() => {});`, { sem: fakeSem, timeoutMs: 300 });
  const elapsed = Date.now() - started;
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /timeout|timed out/i);
  assert.ok(elapsed < 5_000, `took ${elapsed}ms to kill a 300ms-budget async hang -- vm.Script's own timeout does not cover awaited work, so this proves the outer race exists`);
});

test("captured output is capped honestly, with a truncation note, instead of growing unbounded", async () => {
  const result = await runInSandbox(
    `for (let i = 0; i < 2000; i++) { console.log("line-" + i + "-" + "x".repeat(40)); }`,
    { sem: fakeSem, outputCapTokens: 100 },
  );
  assert.equal(result.truncated, true, JSON.stringify(result).slice(0, 200));
  // ~4 chars/token heuristic used throughout this package (see sem-outline.ts) -> ~400 chars, plus a short note.
  assert.ok(result.output.length < 600, `output was ${result.output.length} chars, cap not honored`);
  assert.match(result.output, /truncat/i);
});

test("a runaway loop of sem.* calls is stopped at the call-count cap instead of running to completion", async () => {
  let calls = 0;
  const countingSem = {
    async ping() {
      calls++;
      return calls;
    },
    log: () => {},
  };

  const result = await runInSandbox(
    `
    let n = 0;
    try {
      for (let i = 0; i < 50; i++) {
        await sem.ping();
        n++;
      }
    } catch (e) {
      return { n, err: e.message };
    }
    return { n, done: true };
    `,
    { sem: countingSem, maxCalls: 5 },
  );

  assert.equal(result.ok, true, JSON.stringify(result));
  const value = result.value as { n: number; err?: string; done?: boolean };
  assert.equal(value.n, 5, `expected exactly 5 calls to succeed before the cap fired, got ${JSON.stringify(value)}`);
  assert.match(value.err ?? "", /call|limit|budget/i);
  assert.equal(calls, 5, "the 6th call must never have run at all, not merely gone unreported");
});

test("sem.log is exempt from the call-count cap -- progress reporting must not itself trip the runaway guard", async () => {
  const result = await runInSandbox(
    `for (let i = 0; i < 20; i++) { sem.log("progress " + i); } return "done";`,
    { sem: fakeSem, maxCalls: 3 },
  );
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(result.value, "done");
});

// The basis for tool.ts's apiCalls/edits reporting (scoring how
// much work went through us vs. a provider-native bypass). Sandbox.ts stays
// generic here -- it doesn't know "edit" is semantically special, it just
// records every call that reached the host function, per function name.
test("calls[] records one entry per sem.* call that reached the host function, per function name, with its outcome", async () => {
  const flakySem = {
    async read() {
      return "ok";
    },
    async edit() {
      throw new Error("refused: changes visibility");
    },
    log: () => {},
  };

  const result = await runInSandbox(
    `
    await sem.read();
    await sem.read();
    try { await sem.edit(); } catch (e) { /* swallow -- script's own error handling */ }
    return "done";
    `,
    { sem: flakySem },
  );

  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(result.calls.length, 3);
  assert.deepEqual(
    result.calls.map((c) => c.fn),
    ["read", "read", "edit"],
  );
  assert.deepEqual(
    result.calls.map((c) => c.ok),
    [true, true, false],
  );
  assert.match(result.calls[2]!.error ?? "", /refused: changes visibility/);
});

test("calls[] never includes sem.log, and a call-cap-rejected attempt (never reaching the host function) is not recorded either", async () => {
  let realCalls = 0;
  const countingSem = {
    async ping() {
      realCalls++;
      return realCalls;
    },
    log: () => {},
  };

  const result = await runInSandbox(
    `
    for (let i = 0; i < 20; i++) { sem.log("noise " + i); }
    let n = 0;
    try {
      for (let i = 0; i < 10; i++) { await sem.ping(); n++; }
    } catch (e) { /* cap trips */ }
    return n;
    `,
    { sem: countingSem, maxCalls: 3 },
  );

  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(result.value, 3, "exactly 3 pings should have succeeded before the cap");
  assert.equal(result.calls.length, 3, "only the 3 calls that actually reached ping() are recorded -- not the rejected 4th attempt, and not any sem.log call");
  assert.ok(
    result.calls.every((c) => c.fn === "ping"),
    `expected only "ping" entries, got: ${JSON.stringify(result.calls)}`,
  );
});

// --- logged-value host-thread escape ---
//
// Same class of vector as sandbox-return-value-toJSON-escape.review.test.ts,
// reached via a LOGGED value's toJSON()/toString() instead of the script's
// own return value. Verified empirically before fixing (throwaway vm
// probes) that vm.Script's native timeout ALREADY covers a host-realm
// JSON.stringify call made synchronously from within an active
// runInContext execution (no preceding await) -- the escape only manifests
// once a real host-boundary await has already been crossed, which is the
// SAME pre-existing, disclosed "busy loop after first await" gap this file
// documents elsewhere (sandbox.ts's own file header), not a new hole. The
// fix (compileVoidTrampoline stringifying context-natively) is applied
// uniformly regardless, for the same reason sanitizeReturnValue exists:
// never call host-realm toJSON/toString on a sandbox-authored value, ever.
test("a logged value's hostile toJSON(), with NO preceding await, fails closed (timeout) instead of hanging the host thread", async () => {
  const started = Date.now();
  const result = await runInSandbox(
    `console.log({ toJSON() { const t0 = Date.now(); while (Date.now() - t0 < 2000) {} return "x"; } });`,
    { sem: fakeSem, timeoutMs: 200 },
  );
  const elapsed = Date.now() - started;
  assert.ok(elapsed < 1000, `runInSandbox took ${elapsed}ms despite timeoutMs:200 -- a logged value's hostile toJSON() ran unbounded. Result: ${JSON.stringify(result).slice(0, 200)}`);
  assert.equal(result.ok, false, JSON.stringify(result));
  assert.match(result.error!.message, /timeout|timed out/i);
});

test("sem.log(hostileObject) is equally protected, not just console.log", async () => {
  const started = Date.now();
  const result = await runInSandbox(
    `sem.log({ toJSON() { const t0 = Date.now(); while (Date.now() - t0 < 2000) {} return "x"; } });`,
    { sem: fakeSem, timeoutMs: 200 },
  );
  const elapsed = Date.now() - started;
  assert.ok(elapsed < 1000, `took ${elapsed}ms`);
  assert.equal(result.ok, false, JSON.stringify(result));
});

test("logged values still capture normally -- the context-native stringification is transparent for well-behaved values", async () => {
  const result = await runInSandbox(`console.log("plain string", 42, { a: 1, b: [1, 2, 3] }); sem.log("progress", { done: true });`, { sem: fakeSem });
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.match(result.output, /plain string 42 \{"a":1,"b":\[1,2,3\]\}/);
  assert.match(result.output, /progress \{"done":true\}/);
});

// --- timeout must REVOKE the API ---
//
// Proven empirically before fixing: a script that awaits something slower
// than the declared timeout, then calls a mutating sem.* function, lands
// that mutation on disk AFTER the tool has already reported "timed out" to
// the caller. Fixed via createRunCancellation's SingleWriterCell split:
// runInSandbox revokes the run the instant its own timeout fires; every
// sem.* trampoline (and, in api.ts, write()/editOne()/edit() immediately
// before their real mutation) refuses any further call once revoked.
test("a script that awaits past the timeout, then calls sem.write, never lands the write -- the file is unchanged even long after the tool already reported timed out", async () => {
  await withTempDir(async (dir) => {
    const cancellation = createRunCancellation();
    const realApi = buildSemApi({ cwd: dir, semBin: "sem", cancellation });
    // A host function slower than the declared timeout, standing in for any
    // real sem.* call that happens to take a while (network, a slow sem
    // invocation, etc) -- the exact shape of the review's repro.
    const api = {
      ...realApi,
      slowThing: async () => {
        await new Promise((resolve) => setTimeout(resolve, 500));
        return "slow-done";
      },
    };

    const filePath = join(dir, "malicious.ts");
    const started = Date.now();
    const result = await runInSandbox(
      `
      await sem.slowThing();
      await sem.write("malicious.ts", "malicious content");
      return "done";
      `,
      { sem: api, timeoutMs: 200, cancellation },
    );
    const elapsed = Date.now() - started;

    assert.equal(result.ok, false, JSON.stringify(result));
    assert.match(result.error!.message, /timed out/i);
    assert.ok(elapsed < 1000, `runInSandbox itself must return promptly (took ${elapsed}ms)`);
    assert.equal(existsSync(filePath), false, "sem.write must never have reached the trampoline at all -- the file must not exist yet");

    // The zombie script is STILL RUNNING in the background at this point
    // (its own slowThing() await hasn't resolved yet) -- wait past that,
    // well beyond the declared timeout, and confirm the write STILL never
    // lands. This is the load-bearing guarantee; refusedAfterResolve
    // (below) is best-effort telemetry, not what makes this safe.
    await new Promise((resolve) => setTimeout(resolve, 800));
    assert.equal(existsSync(filePath), false, "the file must still not exist long after the tool already returned -- the zombie's sem.write must have been refused when it finally tried");
  });
});

// A REAL zombie's revoked-call timing is inherently racy to pin down in a
// deterministic unit test (see the test above's own wall-clock proof of
// the load-bearing guarantee instead). What IS fully deterministic: the
// SAME counting mechanism, exercised directly by revoking BEFORE the run
// starts at all -- every sem.* call attempt is refused from the first one,
// with no timing dependency whatsoever.
test("refusedAfterResolve counts every sem.* call attempt refused by an already-revoked cancellation, deterministically", async () => {
  const cancellation = createRunCancellation();
  cancellation.revoke(); // revoked before runInSandbox is even called
  const result = await runInSandbox(
    `
    let refused = 0;
    for (let i = 0; i < 3; i++) {
      try { await sem.ping(); } catch (e) { refused++; }
    }
    return refused;
    `,
    { sem: { ping: async () => "pong", log: () => {} }, cancellation },
  );
  assert.equal(result.ok, true, JSON.stringify(result), "the script itself catches each refusal and completes normally");
  assert.equal(result.value, 3, "all 3 attempts should have been refused, from the very first one");
  assert.equal(result.calls.length, 0, "none of the refused attempts ever reached the host function");
  assert.equal(result.refusedAfterResolve, 3);
});
