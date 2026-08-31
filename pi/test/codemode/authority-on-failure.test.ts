import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { runInSandbox, createRunCancellation } from "../../src/codemode/sandbox.ts";
import { buildSemApi, createChangeLog } from "../../src/codemode/api.ts";

/**
 * team-lead's "authority on failure" audit, extended to the code-mode
 * sandbox (follow-up to the bridge-side audit, 50b4374). READ-ONLY with
 * respect to src/codemode/** -- pisem-codemode owns those files and is
 * mid-matrix; this file is findings + RED tests only, handed off for
 * pisem-codemode to fix in their own files.
 *
 * Two modes already have EXTENSIVE, load-bearing coverage cited here
 * rather than duplicated:
 *  - weave-mcp unavailable/crashing mid-edit (claim held, update fails,
 *    release fails) -- test/tools/weave-edit-lock-leak.test.ts ("a throw
 *    inside the file-mutation-queue callback (after a successful claim)
 *    still releases the weave-mcp claim") and weave-edit-rename-release
 *    .test.ts / weave-edit-rename-update-throws.test.ts. sem.edit()/
 *    sem.rename() are thin wrappers over the exact same performWeaveEdit/
 *    performRename functions weave_edit (native) uses, so this coverage
 *    transfers identically -- there is nothing code-mode-SPECIFIC about
 *    claim/release/rollback.
 *  - atomic batch rollback itself failing partway -- test/tools/
 *    weave-edit-batch-atomic-resync.test.ts, specifically "atomic rollback
 *    resync reports resynced:false with a reason when the resync's own
 *    release call fails" -- reported honestly (a reason string), not
 *    silently, and again shared infrastructure, not code-mode-specific.
 *  - per-run/session budget exhausted mid-script -- withSpend/
 *    applyRowBudget/withDedup (api.ts) NEVER throw or refuse on budget;
 *    they only ever make a result MORE conservative (headers-only, top-5
 *    rows) -- monotonically restrictive by construction, same shape as
 *    PI_SEM_STRICT's argument on the bridge side. test/codemode/
 *    api-budget.test.ts's "once the budget is exhausted, a call that
 *    returns almost nothing may be truncated to zero rows, not throw" and
 *    api-session-budget.test.ts's ceiling tests already prove this
 *    concretely, including one full runInSandbox e2e case
 *    (api-budget.test.ts's "e2e: sem_code's execute() result reports
 *    budget:{used,total} from a real sandboxed run").
 *
 * New findings below, most severe first.
 */

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-authority-on-failure-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

// ============================================================================
// FINDING (new, not previously disclosed): a script that completes NORMALLY
// (no timeout at all) but leaves an unawaited async chain running still
// mutates state in the background, with ZERO reporting to the model and
// ZERO cancellation protection -- `cancellation.revoke()` is ONLY ever
// called by runInSandbox's own timeout race (sandbox.ts line ~493); a run
// that finishes on time never revokes anything. The disclosed "zombie
// script" gap sandbox.ts's header describes, and the existing test in
// sandbox-limits.test.ts ("a script that awaits past the timeout, then
// calls sem.write, never lands the write"), are BOTH specifically about a
// call made AFTER the timeout already fired -- the trampoline's isRevoked()
// check (checked first, before even the call-cap) correctly blocks that.
// This is a DIFFERENT, unaddressed case: the call is made BEFORE the script
// returns (so the trampoline's check passes normally), the script itself
// returns/resolves promptly (ok:true, no timeout, nothing revoked), and the
// unawaited call's OWN internal latency (a slow weave-mcp round-trip, a
// slow disk write, network jitter -- anything) means it finishes AFTER
// tool.ts's execute() has already read `result.calls`/built the tool's
// response and returned it to pi/the model.
//
// Authority verdict: the write() call itself carries no MORE authority
// than any other sem.write() call always has (same write-audit gate, same
// strict-mode enforcement -- see the PI_SEM_STRICT test below). What was
// wrong is REPORTING: a "successful, complete" tool response could have a
// real mutation still pending behind it, invisible to callCount/calls/
// edits telemetry and therefore to the model, with no way for the model to
// know a write it never confirmed might still land. This is exactly team-
// lead's "a silent partial write is as bad as an over-grant" framing --
// here it's not partial (writeFile is atomic at the Node API level, proven
// unreachable to interrupt from JS), it's a SILENT FULL write with a
// falsely-clean "done" already reported.
//
// FIXED (team-lead's fix-shape correction, two parts): the FIRST,
// already-in-flight call's mutation is UNCHANGED by design (this file
// stays a domain-agnostic sandbox; it correctly doesn't block a normal
// return waiting on a call the script itself never awaited) -- what
// changed is that it is no longer INVISIBLE. `pendingAtResolve`
// (sandbox.ts) tracks every sem.* call that reached the host function but
// hadn't settled at return time, and tool.ts's execute() surfaces a
// WARNING line plus `details.pendingAtResolve` whenever it's nonzero, so
// "done" is never falsely clean again. SECOND: `cancellation.revoke()`
// now fires on EVERY resolution (success, error, AND timeout), not only
// timeout as this finding originally described -- this closes the
// CASCADING case (a second sem.* call chained in a `.then()` after the
// first settles is now refused, tracked as `refusedAfterResolve`), even
// though it can't retroactively stop the first call already past the
// trampoline's check by the time revoke() fires.
// ============================================================================

test("FIXED (was FINDING): an unawaited sem.write() left running in the background still lands on disk AFTER a normal (non-timeout) run has reported success -- but is now honestly reported via pendingAtResolve, not silent", async () => {
  await withTempDir(async (dir) => {
    const cancellation = createRunCancellation();

    // A raw (not api.ts's real write()) mock standing in for any
    // real-world async latency (a slow weave-mcp coordination round-trip,
    // disk contention, network) -- the delay itself is not the point; what
    // matters is that it resolves AFTER the script's own top-level code
    // (which never awaits it) has already returned. Deliberately NOT
    // api.ts's own write(), which has its OWN separate, downstream
    // `assertNotRevoked` checkpoint: since that check only runs once
    // THIS mock's own delay elapses, and the fix below now revokes on
    // every resolution (not just timeout), routing through the real
    // write() here would have its checkpoint see the ALREADY-revoked
    // cell and refuse BEFORE ever reaching disk -- a real, and actually
    // stronger, protection this fix provides, but not what THIS test is
    // isolating: sandbox.ts's own pendingAtResolve/revoke semantics for a
    // call that already passed every check it's going to see, and is now
    // purely waiting on unprotected async I/O.
    const filePath = join(dir, "background-write.txt");
    const delayedWrite = async (path: string, content: string) => {
      await new Promise((resolve) => setTimeout(resolve, 80));
      const { writeFile } = await import("node:fs/promises");
      await writeFile(join(dir, path), content, "utf8");
      return { path, bytes: content.length };
    };

    const returnedAt0 = Date.now();
    const result = await runInSandbox(
      `
      // Deliberately NOT awaited -- fire and forget, the exact shape a
      // script gets for free with no special API: any sem.* call whose
      // returned promise is simply never awaited.
      sem.write("background-write.txt", "this lands after the tool already reported done", {}).then(() => {});
      return "script returned immediately, did not await the write";
      `,
      { sem: { write: delayedWrite, log: () => {} }, timeoutMs: 5000, cancellation },
    );
    const elapsedToReturn = Date.now() - returnedAt0;

    // The run reports SUCCESS -- not a timeout, not a partial-calls
    // warning. Nothing about this response tells the model a write is
    // still pending.
    assert.equal(result.ok, true, JSON.stringify(result));
    assert.equal(result.value, "script returned immediately, did not await the write");
    assert.ok(elapsedToReturn < 500, `runInSandbox should return promptly, well before the 80ms background write resolves -- took ${elapsedToReturn}ms`);
    assert.deepEqual(result.calls, [], "at the moment the tool reports success, the background write has not even settled into calls[] -- the histogram/apiCallSequence side of telemetry genuinely shows nothing happened");
    assert.equal(existsSync(filePath), false, "the file must not exist yet at the moment the tool reports success -- proving this is a REAL race, not something already landed");
    // FIX: pendingAtResolve is what makes this no longer invisible --
    // exactly the call calls[] can't see yet is counted here.
    assert.equal(result.pendingAtResolve, 1, "the in-flight write must be counted as pending even though it hasn't settled into calls[] yet -- this is the honest signal calls[]/callCount alone can't provide");

    // Nothing revoked this run -- it completed normally. This is the crux:
    // there is no mechanism watching for "did every sem.* call this script
    // started actually finish before I report."
    // FIX: cancellation.revoke() now fires on EVERY resolution (not just
    // timeout) -- this run completed normally, so it's revoked too. That
    // still doesn't retroactively stop THIS already-in-flight call (see
    // the assertions below -- the write lands anyway), but it DOES mean
    // a second, cascading call chained after this one settles would now
    // be refused rather than silently succeeding too.
    assert.equal(cancellation.isRevoked(), true, "a normal completion now revokes too, closing the cascading-follow-up-call gap -- it just can't stop THIS already-in-flight call retroactively");

    // Wait past the background write's own delay and confirm it silently
    // completes -- a real mutation, on disk, that the already-returned
    // tool response never accounted for.
    await new Promise((resolve) => setTimeout(resolve, 300));
    assert.equal(existsSync(filePath), true, "the background write DOES land -- proving the mutation happens for real, invisibly, after 'done' was already reported");
    assert.equal(readFileSync(filePath, "utf8"), "this lands after the tool already reported done");
  });
});

// ============================================================================
// FINDING (narrower instance of the DISCLOSED gap, now proven deterministically
// at api.ts's own layer rather than only via sandbox.ts's timing-dependent
// test): a write() call that has ALREADY passed its own `assertNotRevoked`
// check completes normally even if the SAME cancellation cell is revoked a
// moment later, while the disk write is still in flight. api.ts's own doc
// comment on SemApiDeps.cancellation already discloses this precisely
// ("narrowing... never fully eliminating... the window between 'already
// committed to an in-flight host call' and 'timeout fires'") -- this test
// makes it concrete and non-flaky (no real wall-clock timeout needed at
// all; the revoke is triggered synchronously, deterministically, right
// after the write() call starts) rather than leaving it as prose.
// ============================================================================

test("FINDING (confirms the disclosed gap, deterministically): a write() call already past its own cancellation check still lands on disk even though the SAME cell is revoked before it resolves", async () => {
  await withTempDir(async (dir) => {
    const cancellation = createRunCancellation();
    const changes = createChangeLog();
    const api = buildSemApi({ cwd: dir, cancellation, changes });

    const filePath = join(dir, "in-flight.txt");

    // write() runs synchronously up to its own `assertNotRevoked(deps)`
    // check (api.ts, immediately before mkdir/writeFile) -- NOT revoked
    // yet, so it passes and the function suspends at its first real await
    // (mkdir). Control returns to us here, synchronously, before that
    // await resolves.
    const writePromise = api.write("in-flight.txt", "content that races the timeout", {});

    // Simulates runInSandbox's timeout handler firing THE INSTANT the
    // write is already committed -- exactly sandbox.ts's own documented
    // "revoke BEFORE rejecting" ordering, just driven directly here for a
    // deterministic (not wall-clock-timing-dependent) proof.
    cancellation.revoke();

    const result = await writePromise;
    assert.deepEqual(result, { path: "in-flight.txt", bytes: "content that races the timeout".length }, "write() reports success -- it has no way to know it was revoked mid-flight, and no second check catches it");
    assert.equal(cancellation.isRevoked(), true, "sanity: the cell really was revoked before write() resolved");
    assert.equal(existsSync(filePath), true, "the write lands on disk anyway -- the SAME race sandbox.ts's own doc comments disclose as narrowed, never fully closed");
    assert.equal(readFileSync(filePath, "utf8"), "content that races the timeout", "not merely present but COMPLETE -- confirms this is a full, honest write racing the report, not partial/corrupted bytes (Node's writeFile is not interruptible mid-syscall from JS)");
  });
});

// ============================================================================
// Confirms PI_SEM_STRICT is NOT weakened by either race above: the
// write-audit refusal check (existing code file + strict mode) is
// evaluated SYNCHRONOUSLY, before assertNotRevoked and before any await --
// so it is never in the race window at all. A "we checked, it's fine"
// result for the one sub-question that could have made the finding above
// materially worse (bypassing strict-mode protection, not just timing).
// ============================================================================

test("PI_SEM_STRICT=1's code-file overwrite refusal is evaluated before any await, so it is NOT vulnerable to the in-flight cancellation race above -- confirmed, not assumed", async () => {
  await withTempDir(async (dir) => {
    const previousStrict = process.env.PI_SEM_STRICT;
    process.env.PI_SEM_STRICT = "1";
    try {
      const filePath = join(dir, "existing.ts");
      const { writeFileSync } = await import("node:fs");
      writeFileSync(filePath, "export const original = 1;\n", "utf8");

      const cancellation = createRunCancellation();
      const changes = createChangeLog();
      const api = buildSemApi({ cwd: dir, cancellation, changes });

      // Attempt to overwrite an EXISTING CODE FILE under strict mode --
      // must refuse, and must refuse before any race window opens (no
      // assertNotRevoked/mkdir/writeFile reached at all).
      const writePromise = api.write("existing.ts", "export const malicious = true;\n", { overwrite: "force" });
      cancellation.revoke(); // even revoking immediately changes nothing -- the refusal already happened synchronously

      await assert.rejects(writePromise, /refuses to overwrite existing code file/);
      assert.equal(readFileSync(filePath, "utf8"), "export const original = 1;\n", "the original content must be completely untouched -- strict mode's refusal is not racy");
    } finally {
      if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
      else process.env.PI_SEM_STRICT = previousStrict;
    }
  });
});

// ============================================================================
// Mode: sem binary missing/erroring mid-script (team-lead: "post the
// runSemJson fix"). Confirms the fix holds for a verb ROUTED THROUGH
// runSemJson (impact) reaching the script as a normal, catchable error --
// not a silent failure, not a hang, not a crash. One message-quality
// observation (not a "silently wrong" finding): a MISSING binary (ENOENT)
// surfaces a raw Node "spawn ... ENOENT" message rather than runSemJson's
// own crafted verbLabel-prefixed text, because runCommand's underlying
// child.on("error", reject) fires before runSemJson's exitCode/stdout
// check ever runs -- still an honest, catchable, non-silent failure, just
// less polished than a non-zero-exit refusal's message. Noted for
// pisem-codemode to decide whether it's worth a fix; not filed as an
// authority or honesty violation since the script/model still gets a
// clear, actionable signal that the binary specifically could not be
// found.
// ============================================================================

test("sem binary entirely missing (ENOENT) surfaces as a normal, catchable script-level error -- not a hang, not silence, not a crash", async () => {
  const api = buildSemApi({ cwd: process.cwd(), semBin: "/no/such/sem-binary-at-all-authority-audit" });

  const started = Date.now();
  const result = await runInSandbox(
    `
    try {
      await sem.impact("someEntity");
      return { caught: false };
    } catch (e) {
      return { caught: true, message: e.message };
    }
    `,
    { sem: { impact: api.impact, log: () => {} }, timeoutMs: 5000 },
  );
  const elapsed = Date.now() - started;

  assert.equal(result.ok, true, JSON.stringify(result));
  assert.ok(elapsed < 5000, `must not hang waiting on a missing binary -- took ${elapsed}ms`);
  const value = result.value as { caught: boolean; message?: string };
  assert.equal(value.caught, true, "the script must be able to catch this as a normal error, not have it hang or crash the run");
  assert.match(value.message ?? "", /ENOENT/);
  assert.deepEqual(result.calls, [{ fn: "impact", ok: false, error: value.message }], "the failed call is honestly recorded in telemetry, same as any other verb failure");
});
