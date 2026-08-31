#!/usr/bin/env node
// verify-pin.mjs -- the one way to declare a pin.
//
// The ebf464a incident: src/tools/internal/rename.ts existed only in one
// session's working tree -- never committed -- while src/codemode/api.ts
// already imported it. Every committed sha up to and including that
// commit's parent was broken in a CLEAN checkout ("Cannot find module"),
// invisible from a working tree where the untracked file papered over it.
// `npx tsc --noEmit` and `npm test`, run against the actual working tree,
// cannot catch this class of bug by construction -- they see whatever is on
// disk, tracked or not. The only thing that catches it is running the same
// checks against a CLEAN, DETACHED checkout of the exact sha being pinned.
// That is this script's whole job: create one, run typecheck + the full
// test suite + a real pi extension load against it, report pass/fail, clean
// up, and exit nonzero on any failure. Run it before declaring ANY sha a
// pin (a release tag, an eval baseline, a "safe to build on" commit for
// another lane) -- see README.md's "Declaring a pin" section.
//
// Dependency-free by design: only node, git, and pi (already required to
// develop/run this repo at all) -- no new npm package.

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, readFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

// This script lives at <package root>/scripts/verify-pin.mjs. The package
// root (where package.json/tsconfig.json/extensions/ live) is not
// necessarily the git repository's top-level directory -- this package can
// be nested inside a larger repo (e.g. <repo>/pi/). `git worktree add`
// always operates on the whole repo, so every step below that needs the
// PACKAGE's own files (npm install/tsc/test/pi load) runs with cwd set to
// the worktree's copy of the package root, not the worktree root itself.
const PKG_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

function usage() {
  console.error("Usage: node scripts/verify-pin.mjs <sha>");
  console.error("");
  console.error("Creates a clean, detached git worktree at <sha>, runs:");
  console.error("  1. npm install");
  console.error("  2. npx tsc --noEmit");
  console.error("  3. npm test");
  console.error("  4. a real `pi --no-extensions -e extensions/pi-sem.ts` load");
  console.error("  5. a static check of the registered sem_*/weave_edit tool surface");
  console.error("Reports pass/fail per step, cleans up the worktree, and exits");
  console.error("nonzero if any step failed.");
}

const shaArg = process.argv[2];
if (!shaArg) {
  usage();
  process.exit(2);
}

function run(cmd, args, opts = {}) {
  return spawnSync(cmd, args, { encoding: "utf8", ...opts });
}

const repoRootResult = run("git", ["rev-parse", "--show-toplevel"], { cwd: PKG_ROOT });
if (repoRootResult.status !== 0) {
  console.error(`verify-pin: not inside a git repository (${(repoRootResult.stderr || "").trim()}).`);
  process.exit(2);
}
const REPO_ROOT = repoRootResult.stdout.trim();
// Relative path from the git repo's top-level to this package's own root
// (e.g. "pi", or "" when the package root IS the repo root) -- rejoined
// onto the worktree below so every package-relative step runs in the right
// place regardless of how deep this package is nested.
const PKG_REL = relative(REPO_ROOT, PKG_ROOT);

const resolveResult = run("git", ["rev-parse", "--verify", `${shaArg}^{commit}`], { cwd: REPO_ROOT });
if (resolveResult.status !== 0) {
  console.error(`verify-pin: "${shaArg}" does not resolve to a commit in this repo.`);
  process.exit(2);
}
const sha = resolveResult.stdout.trim();

const worktreeDir = mkdtempSync(join(tmpdir(), "pi-sem-verify-pin-"));
// This package's own root inside the fresh worktree -- see PKG_ROOT/PKG_REL above.
const worktreePkgDir = join(worktreeDir, PKG_REL);

const steps = [];
let ok = true;

/** Runs `fn` (which must return {ok, detail}) unless a prior step already
 * failed -- fail-fast, same as any ordinary CI gate: no point paying for
 * npm install/tsc/the full suite/a pi load after the first red step, and a
 * later step's "pass" would say nothing trustworthy once an earlier one is
 * already known broken. */
function record(name, fn) {
  if (!ok) {
    steps.push({ name, skipped: true });
    return;
  }
  const start = Date.now();
  const result = fn();
  steps.push({ name, ...result, ms: Date.now() - start });
  if (!result.ok) ok = false;
}

try {
  record("git worktree add --detach", () => {
    const r = run("git", ["worktree", "add", "--detach", worktreeDir, sha], { cwd: REPO_ROOT });
    return { ok: r.status === 0, detail: r.status === 0 ? "" : (r.stderr || r.stdout || "").trim() };
  });

  record("npm install", () => {
    const r = run("npm", ["install", "--no-audit", "--no-fund"], { cwd: worktreePkgDir, timeout: 180_000 });
    return { ok: r.status === 0, detail: r.status === 0 ? "" : (r.stderr || r.stdout || "").slice(-4000) };
  });

  record("npx tsc --noEmit", () => {
    const r = run("npx", ["tsc", "--noEmit"], { cwd: worktreePkgDir, timeout: 120_000 });
    return { ok: r.status === 0, detail: r.status === 0 ? "" : (r.stdout || r.stderr || "").slice(-6000) };
  });

  record("npm test", () => {
    const r = run("npm", ["test"], { cwd: worktreePkgDir, timeout: 180_000 });
    return { ok: r.status === 0, detail: r.status === 0 ? "" : (r.stdout || r.stderr || "").slice(-6000) };
  });

  // A generous 60s timeout: a real (non-worktree) run of this exact command
  // normally completes in well under a second, but was observed to hang
  // once transiently on this machine for an unrelated reason -- generous
  // enough to absorb that, while a GENUINELY broken extension (a committed
  // file importing something never committed, replayed against the real
  // ebf464a~1 sha) was confirmed to fail FAST and cleanly (exit 1, "Cannot
  // find module") rather than hang, so this step is not usually the one
  // burning the timeout budget. A signal/timeout is still treated as a
  // failure, not a pass-by-silence, in case some other regression DOES hang.
  record("pi --no-extensions -e extensions/pi-sem.ts -p /pi-sem --mode json", () => {
    const extPath = join(worktreePkgDir, "extensions", "pi-sem.ts");
    if (!existsSync(extPath)) return { ok: false, detail: `extensions/pi-sem.ts not found at ${extPath}` };
    const r = run("pi", ["--no-extensions", "-e", extPath, "-p", "/pi-sem", "--mode", "json"], {
      cwd: worktreePkgDir,
      timeout: 60_000,
    });
    const combined = `${r.stdout ?? ""}\n${r.stderr ?? ""}`;
    if (r.error) return { ok: false, detail: `spawn error: ${r.error.message}` };
    if (r.signal) return { ok: false, detail: `killed by signal ${r.signal} (timed out after 60s -- a hang is itself a failure here)` };
    if (r.status !== 0) return { ok: false, detail: `exit ${r.status}\n${combined.trim()}` };
    if (/cannot find module|failed to load extension/i.test(combined)) return { ok: false, detail: combined.trim() };
    return { ok: true, detail: "" };
  });

  // Static, not dynamic: pi's own `ctx.ui.notify` (what extensions/pi-sem.ts's
  // own /pi-sem command uses to report its registered tool set) is silent in
  // --mode json/-p print mode (see README's "Known limitations" -- confirmed
  // there against pi's own docs and empirically), and getting a real dynamic
  // tool list out of a non-interactive `pi` invocation would mean either a
  // paid model completion (costly, non-deterministic, needs API credentials
  // this script has no business requiring) or a much heavier RPC-client
  // harness -- disproportionate for a pin guard. This is the pragmatic
  // middle: confirm the exact tool-name constants pi-sem registers are all
  // present in source, and that pi-sem's OWN file never itself declares any
  // of the tool names README.md documents as the live
  // npm:@howaboua/pi-codex-conversion risk (confirmed installed via `pi
  // list` on this machine) -- --no-extensions above is what actually keeps
  // that PACKAGE's tools out at runtime; this is a sanity check that
  // pi-sem doesn't reintroduce the same shapes itself.
  record("tool surface (static): sem_*/weave_edit present, no codex-conversion tool names", () => {
    const extPath = join(worktreePkgDir, "extensions", "pi-sem.ts");
    let source;
    try {
      source = readFileSync(extPath, "utf8");
    } catch (err) {
      return { ok: false, detail: `could not read extensions/pi-sem.ts: ${err instanceof Error ? err.message : String(err)}` };
    }

    const expectedToolNames = [
      "weave_edit",
      "sem_outline",
      "sem_read",
      "sem_find",
      "sem_grep",
      "sem_callers",
      "sem_graph",
      "sem_path",
      "sem_hotspots",
      "sem_cochange",
    ];
    const missing = expectedToolNames.filter((name) => !source.includes(`"${name}"`));
    if (missing.length > 0) {
      return { ok: false, detail: `expected tool name(s) not found in extensions/pi-sem.ts: ${missing.join(", ")}` };
    }

    const codexToolNamePattern = /registerTool\(\s*\{\s*name:\s*["'](exec_command|write_stdin|apply_patch|web_run|imagegen|view_image)["']/;
    const codexMatch = codexToolNamePattern.exec(source);
    if (codexMatch) {
      return { ok: false, detail: `extensions/pi-sem.ts itself registers a codex-conversion-shaped tool name: "${codexMatch[1]}"` };
    }

    return { ok: true, detail: "" };
  });
} finally {
  run("git", ["worktree", "remove", "--force", worktreeDir], { cwd: REPO_ROOT });
  rmSync(worktreeDir, { recursive: true, force: true });
}

const width = Math.max(...steps.map((s) => s.name.length));
for (const s of steps) {
  const label = s.name.padEnd(width);
  if (s.skipped) {
    console.log(`  SKIP  ${label}`);
  } else if (s.ok) {
    console.log(`  PASS  ${label}  (${s.ms}ms)`);
  } else {
    console.log(`  FAIL  ${label}  (${s.ms}ms)`);
    if (s.detail) {
      for (const line of s.detail.split("\n")) console.log(`        ${line}`);
    }
  }
}

if (ok) {
  console.log(`\nverify-pin: PASS -- ${sha} is a clean pin.`);
  process.exit(0);
} else {
  console.log(`\nverify-pin: FAIL -- ${sha} is NOT a clean pin. See the failing step above.`);
  process.exit(1);
}
