// LAW 1 -- NO SILENT LOSS (outcome totality of the edit pipeline).
//
// STRUCTURE: performOneWeaveEdit is optimistic concurrency control with
// backward validation (Kung & Robinson 1981, "On Optimistic Methods for
// Concurrency Control", ACM TODS 6(2): read phase -> validation phase ->
// write phase, retry on validation failure) whose commit is certified by a
// post-write re-read rather than by mutual exclusion, composed with a
// compensating action on verification failure (Garcia-Molina & Salem 1987,
// "Sagas", SIGMOD). The law this file witnesses is the TOTALITY of its
// denotation: ⟦edit⟧ : Request -> Landed + Conflict + Refusal + Error, a
// coproduct of receipts -- every call terminates in EXACTLY ONE summand,
// and the Landed summand is sound (the bytes are on disk).
//
// Static half of the proof (recorded here, executable half below):
// performOneWeaveEdit's QueueOutcome is a CLOSED union of 8 constructors --
// resolution-failed | verification-failed | identity-changed |
// merge-conflict | merge-dropped-ours | write-window-lost |
// rollback-window-lost | success -- and the tail of the function is a total
// case analysis mapping each constructor to exactly one formatter; the only
// other exits are the early "content required" refusal and the queueThrew
// rethrow (Error). Every assignment site of queueOutcome is immediately
// followed by `return`, and a callback that exits without assigning can only
// have thrown (captured as queueThrew). So the enumeration below is the
// WHOLE codomain; a new QueueOutcome constructor would surface as an
// unclassifiable outcome and turn this suite red.
//
// ENUMERATION CHANGE (guarded abort): `rollback-window-lost` is the 8th
// constructor, added when the saga compensation was symmetrized with the
// commit -- the abort path can now REFUSE, reporting that the file moved
// underneath it rather than blind-firing a pre-edit snapshot over a foreign
// process's completed write. It is a refusal summand: isError, nothing
// written, nothing destroyed, `rolledBack: false` telling the truth. The
// classifier below is extended for it deliberately, and the shape pin at the
// end of this file keeps that extension honest. Its deterministic drivers
// (verification-rollback, identity-refusal restore, atomic-batch restore)
// live in test/laws/rollback-guardedness.law.test.ts and are not duplicated
// here -- they need the gated-sem slow-writer seam, which this file's
// per-summand drivers deliberately do without.
//
// The cross-process race half of this law (two free-running OS processes)
// is already witnessed in test/tools/weave-write-window-race-live.test.ts;
// this file deliberately does not duplicate it -- it drives each summand
// deterministically instead, including write-window-lost, which no other
// deterministic test reaches (via the hot-writer fake, which lands a
// foreign write inside every gate round trip).
//
// Non-vacuity / positive control: classify() THROWS on any outcome shape
// outside the coproduct, and the last test feeds it a forged
// success-without-bytes receipt to prove the silent-loss detector actually
// fires (a classifier that cannot go red witnesses nothing).
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { performWeaveEdit, type WeaveEditParams, type WeaveEditOutcome } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const CONFLICT_FAKE = join(__dirname, "..", "tools", "fixtures", "fake-weave-merge-backstop-server.mjs");
const HOT_WRITER_FAKE = join(__dirname, "fixtures", "fake-weave-hot-writer-server.mjs");

const ALPHA = "export function alpha(x: number): number {\n  return x + 1;\n}";
const ALPHA_NEW = "export function alpha(x: number): number {\n  return x + 42;\n}";
const BETA = "export function beta(y: number): number {\n  return y * 2;\n}";
const FILE_V0 = `${ALPHA}\n\n${BETA}\n`;

function makeDir(git: boolean): string {
  const dir = mkdtempSync(join(tmpdir(), "law-no-silent-loss-"));
  writeFileSync(join(dir, "calc.ts"), FILE_V0);
  if (git) {
    execFileSync("git", ["init", "-q", "-b", "main"], { cwd: dir });
    execFileSync("git", ["add", "-A"], { cwd: dir });
    execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  }
  return dir;
}

type Receipt = "landed" | "reported-conflict" | "reported-refusal" | "reported-error";

/**
 * The law's classifier: maps an outcome (or a throw) to exactly one summand
 * of the receipt coproduct, checking the Landed summand's soundness against
 * the actual bytes on disk. Throws on anything unclassifiable -- an outcome
 * outside the enumeration IS the law violation.
 */
function classify(outcome: WeaveEditOutcome | { thrown: unknown }, disk: string, mustContain?: string): Receipt {
  if ("thrown" in outcome) return "reported-error";
  const d = outcome.details;
  if (!outcome.isError) {
    if (!(d.verification as { ok?: boolean } | undefined)?.ok) throw new Error(`success receipt without verification.ok: ${outcome.text}`);
    if (mustContain !== undefined && !disk.includes(mustContain)) {
      throw new Error(`SILENT LOSS: success receipt but the edit's text is not on disk. Receipt: ${outcome.text}`);
    }
    return "landed";
  }
  if (Array.isArray(d.mergeConflicts)) return "reported-conflict";
  // Every refusal shape names itself in details; enumerate them all.
  const isRefusal =
    d.resolved === false || // not-found / ambiguous
    d.rolledBack === true || // verification-failed / identity-changed
    (d.verification as { ok?: boolean } | undefined)?.ok === false ||
    (d.identityChange as { ok?: boolean } | undefined)?.ok === false ||
    (d.mergeGuard as { ok?: boolean } | undefined)?.ok === false || // merge-dropped-ours
    (d.writeWindow as { ok?: boolean } | undefined)?.ok === false || // write-window-lost
    (d.rollbackWindow as { ok?: boolean } | undefined)?.ok === false || // rollback-window-lost
    typeof d.error === "string" || // missing file/entity/op param shape
    /is required for op|pass either/.test(outcome.text); // early param refusals
  if (isRefusal) return "reported-refusal";
  throw new Error(`outcome outside the receipt coproduct -- enumeration is no longer total: ${JSON.stringify({ text: outcome.text, details: d })}`);
}

async function run(params: WeaveEditParams, cwd: string, coordinator?: Coordinator): Promise<WeaveEditOutcome | { thrown: unknown }> {
  try {
    return await performWeaveEdit(params, { cwd, semBin: "sem", coordinator });
  } catch (err) {
    return { thrown: err };
  }
}

const EDIT_ALPHA: WeaveEditParams = { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_NEW };

test("summand 1 (landed): a plain successful replace classifies as landed, with its bytes verified on disk", async () => {
  const dir = makeDir(false);
  try {
    const outcome = await run(EDIT_ALPHA, dir);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.equal(classify(outcome, disk, "x + 42"), "landed");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("summand 3 (refusal): not-found, ambiguous, and missing-content each classify as reported-refusal, disk untouched", async () => {
  const dir = makeDir(false);
  try {
    writeFileSync(join(dir, "two.ts"), "export function dup(): number {\n  return 1;\n}\nexport function dup(): number {\n  return 2;\n}\n");
    const cases: WeaveEditParams[] = [
      { file: "calc.ts", entity: { name: "gamma" }, op: "replace", content: "export function gamma(): number {\n  return 0;\n}" },
      { file: "two.ts", entity: { name: "dup" }, op: "replace", content: "export function dup(): number {\n  return 3;\n}" },
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace" }, // content missing
      {}, // no file/entity/op at all
    ];
    for (const params of cases) {
      const outcome = await run(params, dir);
      assert.equal(classify(outcome, readFileSync(join(dir, "calc.ts"), "utf8")), "reported-refusal", JSON.stringify(params));
    }
    assert.equal(readFileSync(join(dir, "calc.ts"), "utf8"), FILE_V0, "no refusal path may leave bytes behind");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("summand 3 (refusal, rolled back): an identity-changing replace without allow_signature_change refuses and restores", async () => {
  const dir = makeDir(false);
  try {
    const outcome = await run(
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: "export function omega(x: number): number {\n  return x + 9;\n}" },
      dir,
    );
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.equal(classify(outcome, disk), "reported-refusal");
    assert.equal(disk, FILE_V0, "rollback restores the pre-edit bytes");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("summand 2 (conflict): a same-entity merge collision classifies as reported-conflict, nothing written", async () => {
  const dir = makeDir(true);
  const coordinator = new Coordinator({
    command: process.execPath,
    args: [CONFLICT_FAKE],
    env: { FAKE_MERGE_MODE: "conflict" },
    cwd: dir,
    agentId: "law-conflict-agent",
  });
  try {
    const outcome = await run(EDIT_ALPHA, dir, coordinator);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.equal(classify(outcome, disk), "reported-conflict");
    assert.equal(disk, FILE_V0);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("summand 3 (refusal): a merge that dropped ours classifies as reported-refusal, nothing written", async () => {
  const dir = makeDir(true);
  const coordinator = new Coordinator({
    command: process.execPath,
    args: [CONFLICT_FAKE],
    env: { FAKE_MERGE_MODE: "dropours" },
    cwd: dir,
    agentId: "law-dropours-agent",
  });
  try {
    const outcome = await run(EDIT_ALPHA, dir, coordinator);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.equal(classify(outcome, disk), "reported-refusal");
    assert.equal(disk, FILE_V0);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("summand 3 (refusal): a permanently hot file exhausts MAX_MERGE_ATTEMPTS and refuses -- never silently loses to the foreign writer", async () => {
  const dir = makeDir(true);
  const callLog = join(dir, "calls.log");
  const coordinator = new Coordinator({
    command: process.execPath,
    args: [HOT_WRITER_FAKE],
    env: { FAKE_HOT_FILE: join(dir, "calc.ts"), FAKE_WEAVE_CALL_LOG: callLog },
    cwd: dir,
    agentId: "law-hot-agent",
  });
  try {
    const outcome = await run(EDIT_ALPHA, dir, coordinator);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.equal(classify(outcome, disk), "reported-refusal");
    assert.ok(!("thrown" in outcome));
    const details = (outcome as WeaveEditOutcome).details;
    assert.deepEqual((details.writeWindow as { attempts?: number }).attempts, 5, "bounded retry: exactly MAX_MERGE_ATTEMPTS passes");
    assert.ok(!disk.includes("x + 42"), "the refusal is honest: this edit's bytes are NOT on disk");
    assert.ok(disk.includes("hot-writer pass"), "non-vacuity: the foreign writer really did land inside every gate window");
    // Written-nowhere paths resync the CRDT with the restored entity text.
    const log = readFileSync(callLog, "utf8");
    assert.match(log, /weave_update_entity_content:resync/, "the CRDT-ahead-of-disk state is resynced after the refusal");
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("summand 4 (error): a missing file surfaces as a thrown error -- reported, never swallowed into a fake success", async () => {
  const dir = makeDir(false);
  try {
    const outcome = await run({ file: "nope.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_NEW }, dir);
    assert.equal(classify(outcome, ""), "reported-error");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("positive control: the classifier itself detects silent loss (a forged success with no bytes on disk goes red)", () => {
  const forged: WeaveEditOutcome = {
    isError: false,
    text: "weave_edit: replace alpha in calc.ts, lines 1-3 -> 1-3. Verification: ok.",
    details: { verification: { ok: true } },
  };
  assert.throws(() => classify(forged, FILE_V0, "x + 42"), /SILENT LOSS/);
});

test("summand 3 (refusal): the rollback-window-lost shape classifies as a refusal, and is NOT mistaken for a completed rollback", () => {
  // Shape pin for the 8th constructor. Its live drivers are in
  // rollback-guardedness.law.test.ts (they need the gated-sem slow-writer
  // seam); what matters HERE is that the enumeration stays total and that
  // this summand is not read as "rolled back" -- the whole point of it is
  // that the compensation did NOT run.
  const receipt: WeaveEditOutcome = {
    isError: true,
    text: "weave_edit: replace on alpha (function, lines 1-3) in calc.ts failed verification (…), but the rollback was SKIPPED -- calc.ts changed underneath this edit.",
    details: {
      file: "calc.ts",
      op: "replace",
      verification: { ok: false, reason: "1 untouched entity … is gone from extraction afterward" },
      rolledBack: false,
      rollbackWindow: {
        ok: false,
        cause: "verification-failed",
        reason: "the file changed underneath this edit between its write and the rollback",
        expected: { bytes: 120, sha1: "a".repeat(40) },
        actual: { bytes: 131, sha1: "b".repeat(40) },
      },
    },
  };
  assert.equal(classify(receipt, FILE_V0), "reported-refusal");
  assert.equal(receipt.details.rolledBack, false, "a skipped compensation must never report itself as rolled back");
});

test("positive control: an outcome outside the enumerated coproduct is rejected, not absorbed", () => {
  const alien: WeaveEditOutcome = { isError: true, text: "weave_edit: something new happened", details: { novel: true } };
  assert.throws(() => classify(alien, FILE_V0), /enumeration is no longer total/);
});
