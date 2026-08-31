import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-api-write-bypass-test-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * OX-REVIEW-3 FINDING (found independently, category (g): "what would make
 * the model escape to native apply_patch or another disabled built-in? --
 * any refusal path that is wrong").
 *
 * This isn't an escape to a DIFFERENT tool -- it's worse: an entirely
 * ungated escape hatch on the SAME sanctioned tool. `sem.edit()` (api.ts's
 * edit(), wrapping performWeaveEdit) carries real safety machinery this
 * whole review campaign has spent three rounds hardening: identity/
 * visibility-change refusal (compareIdentity/deriveVisibility), dependents
 * reporting, post-write verification with automatic rollback. `sem.write()`
 * (api.ts, lines ~236-244) has NONE of that -- it is a raw filesystem
 * overwrite gated by exactly one check:
 *
 *   if (existsSync(absPath) && !opts.overwrite) {
 *     throw new Error(`sem.write: refuses to overwrite existing file...`);
 *   }
 *
 * `opts.overwrite` is a plain, self-controlled boolean the SCRIPT ITSELF
 * passes -- unlike the tools-mode `write` builtin (extensions/pi-sem.ts),
 * which is wrapped by write-audit.ts and gated by the PI_SEM_STRICT
 * environment variable (something the model cannot toggle itself), code
 * mode's `sem.write()` has ZERO connection to write-audit.ts, ZERO
 * PI_SEM_STRICT gating, and ZERO code-file awareness -- confirmed by grep:
 * neither `write-audit`, `auditWriteCommand`, `isCodeFilePath`, nor
 * `PI_SEM_STRICT` appears anywhere under src/codemode/. A script that wants
 * to bypass every one of sem.edit()'s protections needs no clever trick at
 * all -- `sem.write(path, newContent, {overwrite: true})` is the single
 * MOST OBVIOUS way to write a file's content in the whole API, and it
 * reopens exactly the live bug (fb44a78: an `export` keyword silently
 * dropped, undetected because sem's own graph doesn't model visibility)
 * every identity check in this codebase exists to catch.
 *
 * Untested: api-write.test.ts's own tests demonstrate `{overwrite: true}`
 * succeeding as a *feature*, never checking whether the overwritten
 * content constitutes a safety regression sem.edit() would have refused.
 *
 * FIXED: sem.write() now routes through write-audit.ts's `auditWriteCommand`
 * (the same classifier/strict-mode gate the builtin `write` tool wraps) and
 * layers a stricter rule on top for code files specifically -- an EXISTING
 * code file's overwrite is refused with "use sem.edit for existing
 * entities" unless PI_SEM_STRICT is off AND the caller passes
 * `{ overwrite: "force" }` (a plain `{ overwrite: true }`, the ordinary/
 * careless call shape, is no longer sufficient for those -- only a
 * non-code file or a brand-new file keeps that simpler contract). A forced
 * overwrite is reported through `deps.onWriteAudit`, same path the builtin
 * write wrapper logs through, so it's visible as a policy bypass rather
 * than an invisible one. See api.ts's `write()` for the implementation.
 */
test("sem.write({overwrite:true}) on an existing code file is refused outright -- use sem.edit for existing entities, not a raw overwrite", async () => {
  await withTempDir(async (dir) => {
    const original = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";
    writeFileSync(join(dir, "math.ts"), original, "utf8");

    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    // The "export" keyword being dropped is exactly the regression class
    // sem.edit()'s identity/visibility check exists to catch (see
    // test/tools/weave-edit-safety.test.ts's "replace that silently drops
    // `export` is refused and rolled back by default") -- sem.write() must
    // never be the easier, ungated way to make the same change.
    const dropped = "function add(a: number, b: number): number {\n  return a + b;\n}\n";
    await assert.rejects(() => api.write("math.ts", dropped, { overwrite: true }), /use sem\.edit for existing entities/);

    const finalContent = readFileSync(join(dir, "math.ts"), "utf8");
    assert.equal(finalContent, original, "a refused sem.write() must never touch the file on disk at all.");
  });
});

test("sem.write({overwrite:\"force\"}) on an existing code file succeeds outside strict mode, and is reported via onWriteAudit as a bypass", async () => {
  await withTempDir(async (dir) => {
    const original = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";
    writeFileSync(join(dir, "math.ts"), original, "utf8");

    const audited: Array<{ path: string; forced: boolean; refused: boolean }> = [];
    const api = buildSemApi({ cwd: dir, semBin: "sem", onWriteAudit: (entry) => audited.push(entry) });

    const dropped = "function add(a: number, b: number): number {\n  return a + b;\n}\n";
    await api.write("math.ts", dropped, { overwrite: "force" });

    assert.equal(readFileSync(join(dir, "math.ts"), "utf8"), dropped);
    assert.equal(audited.length, 1);
    assert.equal(audited[0]?.forced, true, "a force-overwrite of an existing code file must be reported as a bypass via onWriteAudit.");
    assert.equal(audited[0]?.refused, false);
  });
});

test("sem.write({overwrite:\"force\"}) on an existing code file is STILL refused under PI_SEM_STRICT -- force is not a strict-mode override", async () => {
  const prior = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_STRICT = "1";
  try {
    await withTempDir(async (dir) => {
      writeFileSync(join(dir, "math.ts"), "export function add(a, b) { return a + b; }\n", "utf8");
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      await assert.rejects(
        () => api.write("math.ts", "function add(a, b) { return a + b; }\n", { overwrite: "force" }),
        /PI_SEM_STRICT/,
      );
    });
  } finally {
    if (prior === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = prior;
  }
});
