import { test } from "node:test";
import assert from "node:assert/strict";
import { runInSandbox } from "../../src/codemode/sandbox.ts";

/**
 * A category "sandbox contract" finding —
 * a NEW, easier-to-trigger relative of the busy-loop-after-await gap
 * DESIGN.md already discloses.
 *
 * src/codemode/sandbox.ts's runInSandbox() (lines ~256-291):
 *
 *   const value = await Promise.race([scriptResultPromise, timeoutPromise]);   // <-- wall-clock timeout race ends HERE
 *   ...
 *   if (value !== undefined) {
 *     try {
 *       value = JSON.parse(JSON.stringify(value));    // <-- runs AFTER the race already resolved successfully, on the HOST's own thread
 *     } catch { value = undefined; }
 *   }
 *
 * `JSON.stringify` calls a value's own `.toJSON()` method if present (a
 * standard, unavoidable part of the JSON.stringify contract — see
 * ECMA-262 §25.5.2.1 SerializeJSONProperty). A script's `return` value
 * crosses back into host code as the resolved value of
 * `script.runInContext(...)`'s promise — a plain object with a `toJSON`
 * method is a completely ordinary, unremarkable value for a script to
 * return; nothing about the sandbox's escape-prevention machinery
 * (trampolines, frozen globals, strict-mode `this`) applies to it, because
 * this isn't a realm-escape attempt — it's the *sandbox's own, entirely
 * expected, function-object return value* being invoked by *host* code,
 * synchronously, on the *host's own single thread*, with no timeout
 * protection whatsoever: the wall-clock race that's supposed to bound
 * every sandbox run already completed by the time this line executes.
 *
 * Net effect: a `sem_code` script needs NO `await` and NO synchronous
 * infinite loop inside its own body at all — DESIGN.md's disclosed gap
 * ("a script that busy-loops synchronously *after* its first `await`
 * cannot be preempted... out of scope for this pass") requires an await
 * first; this vector requires nothing but a `return` statement, and hangs
 * the ENTIRE node process (not just this one tool call — Node is
 * single-threaded) for as long as the malicious `toJSON()` body runs,
 * completely bypassing `timeout_ms` regardless of how small it's set.
 *
 * Confirmed empirically: a 500ms-budget sandbox run whose returned value
 * has a 3-second-busy-loop `toJSON()` took the full 3+ seconds, not ~500ms.
 * With `while(true){}` instead of a bounded loop, this hangs the process
 * indefinitely — every other tool call, every other concurrent request,
 * everything, since there is only one JS thread.
 *
 * Untested: sandbox-limits.test.ts's timeout tests both put the hang
 * *inside the script's own execution* (a `while(true){}` statement, an
 * awaited never-settling promise) — neither returns a crafted object whose
 * serialization is what hangs.
 *
 * Fix sketch: never JSON.stringify a value straight from the sandbox on
 * the host's own thread without its own timeout guard — e.g., race the
 * stringify step itself (impossible to truly preempt synchronously, so
 * this really needs the same fix DESIGN.md already prescribes for the
 * disclosed gap: run the whole sandbox, including result serialization, on
 * a separate `node:worker_threads` worker that can be forcibly terminated
 * on timeout), or strip/ignore any `toJSON` method before serializing
 * (`JSON.stringify(value, (k, v) => v)` doesn't help — replacer runs after
 * toJSON already fired; would need a structural clone that never invokes
 * user-defined methods, e.g. `structuredClone` — though that has its own
 * cross-realm caveats worth verifying before relying on it).
 */
test("a script's returned toJSON() runs on the host thread, completely unbounded by timeout_ms, once the sandbox's own timeout race has already resolved", async () => {
  const started = Date.now();
  const result = await runInSandbox(
    `return { toJSON() { const t0 = Date.now(); while (Date.now() - t0 < 2000) {} return "done"; } };`,
    { sem: { log: () => {} }, timeoutMs: 200 },
  );
  const elapsed = Date.now() - started;

  assert.ok(
    elapsed < 1000,
    `runInSandbox took ${elapsed}ms despite timeoutMs:200 — a value returned from the script ran a 2-second busy-loop inside its own ` +
      `toJSON() during the host's post-timeout JSON.stringify(value) step, completely unbounded by the wall-clock timeout. Result: ${JSON.stringify(result).slice(0, 200)}`,
  );
});
