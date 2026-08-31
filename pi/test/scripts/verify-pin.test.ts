import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..", "..");
const SCRIPT = join(REPO_ROOT, "scripts", "verify-pin.mjs");

/**
 * The ebf464a incident (from this tooling's original repository, before
 * this file moved here): a committed file imported a module that existed
 * only in the working tree, never committed -- so every committed sha up
 * to and including that commit's parent was broken (the extension fails
 * to load: "Cannot find module") in a clean checkout, invisible from the
 * working tree where the untracked file papered over it. The sha below was
 * exactly that broken commit -- a real, deterministic, zero-setup RED
 * fixture for "verify:pin must fail on a sha with a deliberately removed
 * file". It is a real ancestor sha in the ORIGINAL repository's history
 * only; the test below skips gracefully wherever it isn't (e.g. after
 * this file moves to a repo with different history), rather than failing
 * on an assumption this file can't guarantee here.
 */
const KNOWN_BROKEN_SHA = "803401619659fe44d0bd481b988f1f125a945431";

/** Whatever HEAD is right now, resolved once per test run rather than
 * hardcoded -- multiple lanes commit to this tree concurrently, and "does
 * verify-pin pass on a sha someone would actually pin today" is a more
 * honest, representative check than freezing an old sha that could drift
 * out of relevance (or, worse, itself later be revealed broken by some
 * future fix to this very script). */
function resolveHeadSha(): string {
  return execFileSync("git", ["rev-parse", "HEAD"], { cwd: REPO_ROOT, encoding: "utf8" }).trim();
}

function currentRepoIsAncestor(sha: string): boolean {
  const result = spawnSync("git", ["merge-base", "--is-ancestor", sha, "HEAD"], { cwd: REPO_ROOT });
  return result.status === 0;
}

test("verify-pin.mjs exists and is executable via node", () => {
  const result = spawnSync(process.execPath, [SCRIPT], { cwd: REPO_ROOT, encoding: "utf8" });
  // No sha argument: must refuse with a usage message, not crash with a
  // stack trace, and must not exit 0 (an accidental no-op pass would be
  // worse than a crash here -- either is wrong, but only one is silent).
  assert.notEqual(result.status, 0, `Got stdout: ${result.stdout}\nstderr: ${result.stderr}`);
  assert.match(result.stdout + result.stderr, /usage/i, `Got stdout: ${result.stdout}\nstderr: ${result.stderr}`);
});

test(
  "verify-pin.mjs fails on a sha where a committed file imports a module that was never committed (the real ebf464a incident, replayed)",
  { timeout: 180_000, skip: !currentRepoIsAncestor(KNOWN_BROKEN_SHA) ? `${KNOWN_BROKEN_SHA} is not an ancestor of HEAD in this repository's history -- this fixture only replays inside the repository the incident actually happened in.` : false },
  () => {
    const result = spawnSync(process.execPath, [SCRIPT, KNOWN_BROKEN_SHA], {
      cwd: REPO_ROOT,
      encoding: "utf8",
      timeout: 170_000,
    });

    assert.notEqual(result.status, 0, `verify-pin must fail on a genuinely broken sha. Got exit ${result.status}\nstdout: ${result.stdout}\nstderr: ${result.stderr}`);
    assert.match(
      result.stdout + result.stderr,
      /cannot find module|failed to load extension/i,
      `Failure output should name the real cause, not just "something failed". Got stdout: ${result.stdout}\nstderr: ${result.stderr}`,
    );

    // Must never leave a dangling worktree behind, success or failure.
    const worktrees = execFileSync("git", ["worktree", "list", "--porcelain"], { cwd: REPO_ROOT, encoding: "utf8" });
    assert.doesNotMatch(worktrees, /verify-pin/, `A verify-pin temp worktree was left behind:\n${worktrees}`);
  },
);

test(
  "verify-pin.mjs passes on the current HEAD",
  { timeout: 300_000 },
  () => {
    const headSha = resolveHeadSha();

    const result = spawnSync(process.execPath, [SCRIPT, headSha], {
      cwd: REPO_ROOT,
      encoding: "utf8",
      timeout: 290_000,
    });

    assert.equal(result.status, 0, `Got exit ${result.status}\nstdout: ${result.stdout}\nstderr: ${result.stderr}`);
    assert.match(result.stdout, /pass/i, `Got stdout: ${result.stdout}`);

    const worktrees = execFileSync("git", ["worktree", "list", "--porcelain"], { cwd: REPO_ROOT, encoding: "utf8" });
    assert.doesNotMatch(worktrees, /verify-pin/, `A verify-pin temp worktree was left behind:\n${worktrees}`);
  },
);
