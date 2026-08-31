import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { execFileSync } from "node:child_process";
import { buildSemApi } from "../../src/codemode/api.ts";

/**
 * v2 item 2: sem.rename() -- a thin code-mode wrapper over the (now landed)
 * performRename engine (src/tools/internal/rename.ts). This file exercises
 * the WRAPPER's contract from inside buildSemApi(): error routing through
 * toCodeModeError (the unified refusal phrasing), ChangeLog integration
 * (matching edit()/write()'s existing changed()-integration pattern), and
 * the real `applied: number` (a site count) return shape -- not the
 * `applied: true` boolean an earlier, pre-implementation sketch guessed
 * at.
 */

function initGitRepo(dir: string): void {
  execFileSync("git", ["init", "-q"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"], { cwd: dir });
}

function commitAll(dir: string): void {
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "fixture"], { cwd: dir });
}

function withTempRepo<T>(build: (dir: string) => void, run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-rename-test-"));
  initGitRepo(dir);
  build(dir);
  commitAll(dir);
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("sem.rename() applies a multi-site rename, returns a real applied: number (not a boolean), and records every touched file into the ChangeLog", async () => {
  await withTempRepo(
    (dir) => {
      writeFileSync(join(dir, "lib.ts"), ["export function computeScore(x: number): number {", "  return x * 2;", "}", ""].join("\n"), "utf8");
      writeFileSync(
        join(dir, "caller.ts"),
        ['import { computeScore } from "./lib.ts";', "", "export function useIt(x: number): number {", "  return computeScore(x) + 1;", "}", ""].join(
          "\n",
        ),
        "utf8",
      );
    },
    async (dir) => {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      const result = (await api.rename("computeScore", "computeRating")) as {
        old_name: string;
        new_name: string;
        applied: number;
        files: string[];
        verified: boolean;
        leftovers: unknown[];
      };

      assert.equal(result.old_name, "computeScore");
      assert.equal(result.new_name, "computeRating");
      assert.equal(typeof result.applied, "number", `applied must be a real site-count number, got ${JSON.stringify(result.applied)}`);
      assert.ok(result.applied >= 3, `expected at least 3 sites (def + import + call), got ${result.applied}`);
      assert.equal(result.verified, true);
      assert.equal(result.leftovers.length, 0);
      assert.deepEqual([...result.files].sort(), ["caller.ts", "lib.ts"]);

      assert.match(readFileSync(join(dir, "lib.ts"), "utf8"), /export function computeRating\(/);
      assert.match(readFileSync(join(dir, "caller.ts"), "utf8"), /import \{ computeRating \} from "\.\/lib\.ts";/);

      const changed = (await api.changed()) as { files: string[]; entries: Array<{ file: string; op: string }> };
      assert.deepEqual([...changed.files].sort(), ["caller.ts", "lib.ts"], `expected both renamed files recorded in the ChangeLog, got ${JSON.stringify(changed)}`);
      assert.ok(
        changed.entries.every((e) => e.op === "rename"),
        `expected every ChangeLog entry from a rename to be tagged op: "rename", got ${JSON.stringify(changed)}`,
      );
    },
  );
});

test("sem.rename() on an unknown entity throws a code-mode-phrased error (item 6 unified routing), not a raw CLI dump", async () => {
  await withTempRepo(
    (dir) => {
      writeFileSync(join(dir, "a.ts"), "export function foo(): number {\n  return 1;\n}\n", "utf8");
    },
    async (dir) => {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      await assert.rejects(
        () => api.rename("totallyMissing", "whatever"),
        (err: Error) => {
          assert.match(err.message, /no entity named "totallyMissing"/i, `Got: ${err.message}`);
          return true;
        },
      );
    },
  );
});

test("sem.rename() on an ambiguous name (two files) refuses with candidates; opts.file disambiguates and leaves the other file untouched", async () => {
  await withTempRepo(
    (dir) => {
      writeFileSync(join(dir, "a.ts"), "export function shared(): number {\n  return 1;\n}\n", "utf8");
      writeFileSync(join(dir, "b.ts"), "export function shared(): number {\n  return 2;\n}\n", "utf8");
    },
    async (dir) => {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });

      await assert.rejects(
        () => api.rename("shared", "sharedRenamed"),
        (err: Error) => {
          assert.match(err.message, /ambiguous/i, `Got: ${err.message}`);
          assert.match(err.message, /a\.ts/);
          assert.match(err.message, /b\.ts/);
          return true;
        },
      );

      const result = (await api.rename("shared", "sharedRenamed", { file: "a.ts" })) as { applied: number; files: string[] };
      assert.ok(result.applied >= 1);
      assert.match(readFileSync(join(dir, "a.ts"), "utf8"), /export function sharedRenamed\(/);
      assert.match(readFileSync(join(dir, "b.ts"), "utf8"), /export function shared\(/);
    },
  );
});

test("sem.rename() spends into both the per-run and session budgets like every other mutator", async () => {
  await withTempRepo(
    (dir) => {
      writeFileSync(join(dir, "lib.ts"), "export function tinyFn(): number {\n  return 1;\n}\n", "utf8");
    },
    async (dir) => {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      const before = ((await api.changed()) as { files: unknown[] }).files.length;
      assert.equal(before, 0);
      await api.rename("tinyFn", "tinyFunc");
      const after = ((await api.changed()) as { files: unknown[] }).files.length;
      assert.equal(after, 1);
    },
  );
});
