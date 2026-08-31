// LAW 4' -- GUARDED COMPENSATION (the symmetric half of LAW 4).
//
// This file was `rollback-unguarded-write.review.test.ts`: a pinned,
// deterministically reproduced OPEN DEFECT with its assertions inverted per
// this repo's *.review.test.ts convention. The defect is now fixed and the
// assertions are flipped in, so this is a law witness, not a reproduction.
//
// ---------------------------------------------------------------------
// THE LAW
//
// performOneWeaveEdit is optimistic concurrency control with backward
// validation (Kung & Robinson 1981) composed with a saga compensation
// (Garcia-Molina & Salem 1987). Before the fix the transaction was
// ASYMMETRIC: the forward commit was validated (compare-adjacent-write plus
// post-write confirmation), the abort was not -- `writeFile(absPath,
// currentContent)` was a blind last-writer-wins restore of a snapshot taken
// before the edit began. A foreign process's COMPLETED write, landing in the
// verification window, was destroyed by that restore: no error, no conflict,
// no receipt anywhere. And the failure verdict that triggered the restore
// could itself be CAUSED by the same overlap, because verification
// re-extracted entities from DISK and a torn read of a foreign mid-write
// file reads as "untouched entities gone".
//
// The law both halves of the fix must satisfy:
//
//   (i)  PURITY OF VERIFICATION. Verification asks a question about the
//        transaction's OWN output, so it is answered from `textToWrite`, in
//        memory, never from disk. A foreign process writing during the
//        verification window therefore cannot manufacture a verdict.
//
//   (ii) GUARDEDNESS OF COMPENSATION. C(T) may only compensate T's own
//        write, never a stranger's. Every engine write -- forward commit,
//        verification rollback, identity-refusal restore, atomic-batch
//        restore -- goes through ONE primitive, guardedWrite(path,
//        expectedImage, bytes), a synchronous compare-adjacent-write. A
//        rollback whose guard is lost does NOT retry blindly: it reports
//        `rollback-window-lost`, naming what it expected and what it found.
//        Nothing is destroyed, and the caller learns the file is hot.
//
// REPRODUCTION SEAM (deterministic -- a controlled slow writer via
// fixtures/laws-gated-sem.mjs, no timing races): gate the engine's
// verification extraction; while it is held, land a foreign writer's bytes
// on the target file; release. Every case below is a fixed interleaving, not
// a race.
//
// RED EVIDENCE: on the pre-fix engine all four cases fail -- the foreign
// writer's completed bytes are gone from disk in each one, replaced by the
// pre-edit snapshot.
// ---------------------------------------------------------------------
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performWeaveEdit, type WeaveEditOutcome } from "../../src/tools/weave-edit.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const LAWS_GATED_SEM = join(__dirname, "fixtures", "laws-gated-sem.mjs");

const ORIGINAL = "export function target(): number {\n  return 1;\n}\n\nexport function other(): number {\n  return 2;\n}\n";
/** A clean replace: parses, keeps target's identity. */
const OURS = "export function target(): number {\n  return 42;\n}";
/** A replace that breaks parsing around the edit -- `other` stops being extractable, so verification GENUINELY fails. */
const OURS_BROKEN = "export function target(): number {\n  return 42;\n  const x = {\n";
/** A replace that renames the entity -- refused (and rolled back) without allow_signature_change. */
const OURS_RENAMED = "export function renamed(): number {\n  return 42;\n}";

// The foreign writer's write, as two deterministic stages of one slow write:
const FOREIGN_TORN = "// foreign writer: first chunk of a larger write\n";
const FOREIGN_FINAL =
  "export function target(): number {\n  return 777; // foreign writer's completed edit\n}\n\nexport function other(): number {\n  return 778;\n}\n";

async function waitForFile(path: string, label: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  while (!existsSync(path)) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((r) => setTimeout(r, 25));
  }
}

interface Harness {
  dir: string;
  signals: string;
}

/**
 * Gates the Nth `sem entities` invocation of the engine. #1 is always the
 * pre-edit extraction of a file; #2 is that edit's verification extraction.
 */
function gate(nth: number, style: "pre" | "mid" | "both", signals: string): void {
  process.env.LAWS_GATED_DIR = signals;
  process.env.LAWS_GATED_REAL = "sem";
  process.env.LAWS_GATE_SUBCOMMAND = "entities";
  process.env.LAWS_GATE_NTH = String(nth);
  process.env.LAWS_GATE_STYLE = style;
}

async function withHarness(body: (h: Harness) => Promise<void>): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "law-guarded-rollback-"));
  const signals = mkdtempSync(join(tmpdir(), "law-guarded-signals-"));
  const savedEnv = { ...process.env };
  try {
    await body({ dir, signals });
  } finally {
    process.env = savedEnv;
    rmSync(dir, { recursive: true, force: true });
    rmSync(signals, { recursive: true, force: true });
  }
}

test("law (i) purity: a torn foreign read during the verification window cannot manufacture a rollback -- the bystander's completed write survives intact", async () => {
  await withHarness(async ({ dir, signals }) => {
    const file = join(dir, "f.ts");
    writeFileSync(file, ORIGINAL);
    // STYLE=both gives two interposition points around the verification
    // extraction: before it runs (land the TORN bytes -- the read that used
    // to produce a false parse-failure verdict) and after it read but before
    // the engine acts (land the COMPLETED foreign write).
    gate(2, "both", signals);

    const pending = performWeaveEdit({ file: "f.ts", entity: { name: "target" }, op: "replace", content: OURS }, { cwd: dir, semBin: LAWS_GATED_SEM, coordinator: undefined });

    await waitForFile(join(signals, "blocked-pre"), "the engine's verification extraction");
    // Non-vacuity: the engine's own forward write really landed first.
    assert.ok(readFileSync(file, "utf8").includes("return 42"), "premise: the spliced edit is on disk before the foreign writer starts");

    writeFileSync(file, FOREIGN_TORN); // stage 1: mid-write (torn) bytes
    writeFileSync(join(signals, "continue-pre"), "");
    await waitForFile(join(signals, "extracted"), "sem to have finished the verification extraction");
    writeFileSync(file, FOREIGN_FINAL); // stage 2: the foreign write COMPLETES
    writeFileSync(join(signals, "continue-post"), "");

    const outcome = await pending;
    const disk = readFileSync(file, "utf8");

    // THE LAW: nothing this edit did destroyed the bystander's bytes.
    assert.equal(disk, FOREIGN_FINAL, "the foreign writer's completed write is untouched");
    assert.ok(disk.includes("777"), "a rollback must never overwrite bytes it did not itself write");

    // ...and verification, now a pure function of textToWrite, never saw the
    // torn bytes at all, so there was no rollback to skip.
    assert.doesNotMatch(outcome.text, /failed verification/i, "a torn foreign read must not produce a verification verdict");
    assert.equal(outcome.details.rolledBack, false, "no compensation ran -- verification passed, so there was nothing to compensate");
    assert.equal(outcome.details.rollbackWindow, undefined, "and none was even attempted, so no rollback guard was consulted");

    // The engine is still honest about its OWN edit: the post-write
    // confirmation reads disk, sees the foreign bytes, and refuses rather
    // than claiming a success whose text is not there.
    assert.equal(outcome.isError, true);
    assert.equal((outcome.details.writeWindow as { ok?: boolean } | undefined)?.ok, false, "the honest receipt is a hot-file refusal, not a spurious rollback");
  });
});

test("law (ii) guardedness, verification rollback: a genuine verification failure over a file that moved underneath reports rollback-window-lost and destroys nothing", async () => {
  await withHarness(async ({ dir, signals }) => {
    const file = join(dir, "f.ts");
    writeFileSync(file, ORIGINAL);
    // STYLE=mid: the verification extraction runs for real (against the
    // engine's own output, which genuinely fails to parse), and only THEN is
    // the foreign write landed -- so the failure verdict is real and the
    // rollback is the thing under test.
    gate(2, "mid", signals);

    const pending = performWeaveEdit(
      { file: "f.ts", entity: { name: "target" }, op: "replace", content: OURS_BROKEN },
      { cwd: dir, semBin: LAWS_GATED_SEM, coordinator: undefined },
    );

    await waitForFile(join(signals, "extracted"), "the engine's verification extraction");
    writeFileSync(file, FOREIGN_FINAL);
    writeFileSync(join(signals, "continue-post"), "");

    const outcome = await pending;
    const disk = readFileSync(file, "utf8");

    assert.equal(disk, FOREIGN_FINAL, "C(T) may only compensate T's own write, never a stranger's");
    assert.ok(disk.includes("777"), "the foreign writer's completed edit survives the aborted transaction");

    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /rollback (was )?skipped|file changed underneath/i, "the skipped compensation is reported, never silent");
    const rw = outcome.details.rollbackWindow as { ok?: boolean; cause?: string; expected?: unknown; actual?: unknown } | undefined;
    assert.equal(rw?.ok, false, "the receipt names the new summand");
    assert.equal(rw?.cause, "verification-failed", "and names which compensation was skipped");
    assert.ok(rw?.expected !== undefined && rw?.actual !== undefined, "the receipt carries what it expected vs what it found");
    assert.equal(outcome.details.rolledBack, false, "rolledBack must tell the truth: nothing was restored");
    // Non-vacuity: the verification failure itself was genuine, not torn-read noise.
    assert.equal((outcome.details.verification as { ok?: boolean }).ok, false);
  });
});

test("law (ii) guardedness, identity-refusal restore: the same guard covers the allow_signature_change refusal path", async () => {
  await withHarness(async ({ dir, signals }) => {
    const file = join(dir, "f.ts");
    writeFileSync(file, ORIGINAL);
    gate(2, "mid", signals);

    const pending = performWeaveEdit(
      { file: "f.ts", entity: { name: "target" }, op: "replace", content: OURS_RENAMED },
      { cwd: dir, semBin: LAWS_GATED_SEM, coordinator: undefined },
    );

    await waitForFile(join(signals, "extracted"), "the engine's verification extraction");
    writeFileSync(file, FOREIGN_FINAL);
    writeFileSync(join(signals, "continue-post"), "");

    const outcome = await pending;
    const disk = readFileSync(file, "utf8");

    assert.equal(disk, FOREIGN_FINAL, "the identity refusal's restore is guarded exactly like the verification rollback");
    assert.ok(disk.includes("777"));
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /rollback (was )?skipped|file changed underneath/i);
    const rw = outcome.details.rollbackWindow as { ok?: boolean; cause?: string } | undefined;
    assert.equal(rw?.ok, false);
    assert.equal(rw?.cause, "identity-changed");
    // Non-vacuity: the refusal itself was genuine (a real rename was detected).
    assert.equal((outcome.details.identityChange as { ok?: boolean }).ok, false);
  });
});

test("law (ii) guardedness, atomic batch restore: the all-or-nothing snapshot restore skips a file a stranger wrote, and says so", async () => {
  await withHarness(async ({ dir, signals }) => {
    const a = join(dir, "a.ts");
    const b = join(dir, "b.ts");
    writeFileSync(a, ORIGINAL);
    writeFileSync(b, ORIGINAL);
    // sem `entities` order across the batch: a-pre (#1), a-verify (#2),
    // b-pre (#3), b-verify (#4). Gate #4 mid: edit b's verification verdict
    // is computed for real, then the foreign write lands on A -- the file the
    // batch rollback is about to restore.
    gate(4, "mid", signals);

    const pending = performWeaveEdit(
      {
        atomic: true,
        edits: [
          { file: "a.ts", entity: { name: "target" }, op: "replace", content: OURS },
          { file: "b.ts", entity: { name: "target" }, op: "replace", content: OURS_BROKEN },
        ],
      },
      { cwd: dir, semBin: LAWS_GATED_SEM, coordinator: undefined },
    );

    await waitForFile(join(signals, "extracted"), "edit 2's verification extraction");
    writeFileSync(a, FOREIGN_FINAL);
    writeFileSync(join(signals, "continue-post"), "");

    const outcome = await pending;

    assert.equal(readFileSync(a, "utf8"), FOREIGN_FINAL, "the batch restore must not clobber a file a stranger wrote");
    assert.ok(readFileSync(a, "utf8").includes("777"));
    // b was never touched by a stranger, so ITS restore proceeds exactly as before.
    assert.equal(readFileSync(b, "utf8"), ORIGINAL, "the unguarded-frame file is still restored byte-for-byte");

    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /restore (was )?skipped|changed underneath/i, "the batch receipt names the file it did not restore");
    const skipped = outcome.details.restoreSkipped as Array<{ file?: string }> | undefined;
    assert.ok(Array.isArray(skipped) && skipped.some((s) => s.file === "a.ts"), "details.restoreSkipped enumerates the unrestored files");
  });
});

test("regression guard: in the single-writer frame the guarded rollback is still an EXACT inverse (guardedWrite did not weaken LAW 4's green half)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "law-guarded-exact-"));
  try {
    const file = join(dir, "f.ts");
    writeFileSync(file, ORIGINAL);
    const outcome: WeaveEditOutcome = await performWeaveEdit(
      { file: "f.ts", entity: { name: "target" }, op: "replace", content: OURS_BROKEN },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /failed verification and was rolled back/);
    assert.equal(outcome.details.rolledBack, true);
    assert.equal(readFileSync(file, "utf8"), ORIGINAL, "no foreign writer means the guard holds and the compensation runs to completion");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
