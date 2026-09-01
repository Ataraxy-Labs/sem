import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSemApi, createCheckCache, detectRunner } from "../../src/codemode/api.ts";
import { runCommand } from "../../src/tools/internal/proc.ts";

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-check-test-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * v2 item 4: check() answers "am I still green" without leaving the
 * sandbox for bash -- detects the project's own typecheck/test command
 * (cargo/npm/pytest/go, never invented), runs typecheck first when both
 * exist (cheap failure surfaces before the usually-slower test run), and
 * reports only the failure. Contract: no-runner case returns {pass:null,
 * reason, try}; accepts an explicit `cmd` override; typecheck-before-test
 * ordering.
 *
 * Execution+ordering+caching tests use the "npm" path exclusively (`node`
 * is always available in this environment; cargo/go/pytest aren't
 * guaranteed, and setting up a real compiling crate/module just to prove
 * ordering would be slow and beside the point) -- cargo/go/pytest get
 * detection-only coverage (pure, no execution) below.
 */

test("check(): no runner detected in an empty directory returns pass:null with reason and try", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, null);
    assert.equal(result.reason, "no cargo/npm/pytest/go runner found");
    // P2d: the no-runner advice now leads with the repo-declared runner
    // (.sem/check.json), the general answer to "this project's verification
    // command isn't one of the four this detector knows".
    assert.equal(result.try, `declare this repo's own commands in .sem/check.json ({"typecheck": "...", "test": "..."}), or sem.check({cmd:'make test'})`);
  });
});

test("check(): a package.json with neither a typecheck nor a test script is treated as no runner, not an invented `npm test`", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: {} }));
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, null);
  });
});

test("check(): { cmd } overrides detection entirely, even when a runner would otherwise be detected -- as long as the override is itself an allowed command (PI_SEM_CHECK_ALLOW extends the allowlist)", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "Cargo.toml"), "[package]\nname = \"x\"\n");
    const prevAllow = process.env.PI_SEM_CHECK_ALLOW;
    process.env.PI_SEM_CHECK_ALLOW = "node";
    try {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      const result = await api.check({ cmd: `node -e "process.exit(0)"` });
      assert.equal(result.pass, true);
      assert.equal(result.runner, undefined, "an explicit cmd override should not report a detected runner kind");
    } finally {
      if (prevAllow === undefined) delete process.env.PI_SEM_CHECK_ALLOW;
      else process.env.PI_SEM_CHECK_ALLOW = prevAllow;
    }
  });
});

// --- cmd allowlist -- pure mode's "no shell" claim only holds if `cmd` is
// constrained to detected project runners + explicit user extensions
// (PI_SEM_CHECK_ALLOW). Without this, { cmd } is an arbitrary-command
// escape hatch inside a mode that otherwise has no bash/write tool.

test("check(): { cmd } naming an arbitrary, non-runner command is REFUSED, not executed", async () => {
  await withTempDir(async (dir) => {
    const marker = join(tmpdir(), `pi-sem-check-allowlist-pwned-${Date.now()}-${Math.random().toString(36).slice(2)}`);
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.check({ cmd: `touch ${marker}` }),
      (err: Error) => {
        assert.match(err.message, /^sem\.check:/);
        assert.match(err.message, /not.*allow/i);
        assert.match(err.message, /PI_SEM_CHECK_ALLOW/);
        return true;
      },
    );
    assert.ok(!existsSync(marker), "the arbitrary command must never have executed");
  });
});

test("check(): { cmd } matching a detected npm/yarn/pnpm/bun prefix is allowed to run", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(0)"` } }));
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check({ cmd: "npm test" });
    assert.equal(result.pass, true);
  });
});

test("check(): { cmd } naming an unlisted cargo subcommand is refused even though Cargo.toml is present -- only test/build/check/clippy are allowed", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "Cargo.toml"), "[package]\nname = \"x\"\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.check({ cmd: "cargo publish" }));
  });
});

test("check(): { cmd } of bare `make` (no target) is refused even with a Makefile present; `make <target>` is allowed", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "Makefile"), "test:\n\ttrue\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.check({ cmd: "make" }));
    const result = await api.check({ cmd: "make test" });
    assert.equal(result.pass, true);
  });
});

test("check(): PI_SEM_CHECK_ALLOW accepts both comma- and colon-separated prefixes", async () => {
  await withTempDir(async (dir) => {
    const prevAllow = process.env.PI_SEM_CHECK_ALLOW;
    process.env.PI_SEM_CHECK_ALLOW = "just test,tox -e py311:node";
    try {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      const result = await api.check({ cmd: `node -e "process.exit(0)"` });
      assert.equal(result.pass, true);
    } finally {
      if (prevAllow === undefined) delete process.env.PI_SEM_CHECK_ALLOW;
      else process.env.PI_SEM_CHECK_ALLOW = prevAllow;
    }
  });
});

test("check(): allowlist detection stats manifests once per cwd (session-cheap), not on every call -- a Makefile added after the first check() isn't picked up by a later call against the same cwd", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.check({ cmd: "make test" }), "no Makefile yet -- refused");
    writeFileSync(join(dir, "Makefile"), "test:\n\ttrue\n");
    await assert.rejects(
      () => api.check({ cmd: "make test" }),
      "Makefile added after the first detection -- still refused because detection is cached per cwd, not re-stat'd per call",
    );
  });
});

test("check(): a passing npm typecheck+test both run, pass:true, stage 'test' (the LAST thing that ran)", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({ name: "x", scripts: { typecheck: `node -e "process.exit(0)"`, test: `node -e "process.exit(0)"` } }),
    );
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, true);
    assert.equal(result.runner, "npm");
    assert.equal(result.stage, "test");
  });
});

test("check(): a FAILING typecheck stops there -- the test script never runs at all", async () => {
  await withTempDir(async (dir) => {
    const marker = join(dir, "test-ran.marker");
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({
        name: "x",
        scripts: {
          typecheck: `node -e "process.exit(1)"`,
          test: `node -e "require('fs').writeFileSync(${JSON.stringify(marker)}, 'ran')"`,
        },
      }),
    );
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, false);
    assert.equal(result.stage, "typecheck", "the failure must be reported as coming from the typecheck stage");
    assert.ok(!existsSync(marker), "the test script must never have run after typecheck already failed");
  });
});

test("check(): a passing typecheck but a failing test reports stage 'test' with tailed failure output", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({
        name: "x",
        scripts: { typecheck: `node -e "process.exit(0)"`, test: `node -e "console.log('boom'); process.exit(1)"` },
      }),
    );
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, false);
    assert.equal(result.stage, "test");
    assert.ok(Array.isArray(result.failed) && result.failed.length > 0);
    assert.ok(result.failed!.some((l) => l.includes("boom")));
    assert.ok(result.failed!.length <= 20, `failed must be capped at 20 lines, got ${result.failed!.length}`);
  });
});

test("check(): an npm project with only a test script (no typecheck) still runs it", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(0)"` } }));
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, true);
    assert.equal(result.stage, "test");
  });
});

// --- detection-only (pure, no execution) for cargo/go/pytest ---
// detectRunner() is deliberately pure (no side effects) so it's cheap and
// fast to test directly, without invoking a real cargo/go/pytest
// toolchain -- execution/ordering/tailing is already proven above via the
// npm path, runner-agnostically (detectRunner just decides WHICH command
// to run; runOneCheckCommand/tailLines don't care which language it was).

test("detectRunner(): a Cargo.toml selects cargo check + cargo test", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "Cargo.toml"), "[package]\nname = \"x\"\n");
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "cargo");
    assert.deepEqual(runner?.typecheckCmd, ["cargo", "check", "--quiet"]);
    assert.deepEqual(runner?.testCmd, ["cargo", "test", "--quiet"]);
  });
});

test("detectRunner(): a go.mod selects go build + go test", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "go.mod"), "module x\n\ngo 1.21\n");
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "go");
    assert.deepEqual(runner?.typecheckCmd, ["go", "build", "./..."]);
    assert.deepEqual(runner?.testCmd, ["go", "test", "./..."]);
  });
});

test("detectRunner(): a pytest.ini selects pytest, with no separate typecheck command", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "pytest.ini"), "[pytest]\n");
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "pytest");
    assert.deepEqual(runner?.testCmd, ["pytest", "-q"]);
    assert.equal(runner?.typecheckCmd, undefined);
  });
});

test("detectRunner(): an npm project with only a typecheck script selects ONLY that, no invented test command", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { typecheck: "tsc --noEmit" } }));
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "npm");
    assert.deepEqual(runner?.typecheckCmd, ["npm", "run", "typecheck"]);
    assert.equal(runner?.testCmd, undefined);
  });
});

test("detectRunner(): an empty directory finds nothing", async () => {
  await withTempDir(async (dir) => {
    const runner = await detectRunner(dir);
    assert.equal(runner, null);
  });
});

test("detectRunner(): cargo is preferred over an npm marker if somehow both are present (Cargo.toml checked first)", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "Cargo.toml"), "[package]\nname = \"x\"\n");
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: "echo" } }));
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "cargo");
  });
});

test("check(): reports the pass:null contract identically regardless of which language markers are absent", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check();
    assert.equal(result.pass, null);
    assert.equal(result.runner, undefined);
    assert.equal(result.failed, undefined);
  });
});

// --- caching (session-scoped, keyed by real git tree state) ---

async function initGitRepo(dir: string): Promise<void> {
  await runCommand("git", ["init", "-q"], dir);
  await runCommand("git", ["config", "user.email", "test@example.com"], dir);
  await runCommand("git", ["config", "user.name", "Test"], dir);
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(0)"` } }));
  await runCommand("git", ["add", "."], dir);
  await runCommand("git", ["commit", "-q", "-m", "init"], dir);
}

test("check(): a second call with an UNCHANGED tree is served from cache", async () => {
  await withTempDir(async (dir) => {
    await initGitRepo(dir);
    const checkCache = createCheckCache();
    const api = buildSemApi({ cwd: dir, semBin: "sem", checkCache });

    const first = await api.check();
    assert.equal(first.pass, true);
    assert.equal(first.cached, undefined, "the first call must actually run the command, not claim to be cached");

    const second = await api.check();
    assert.equal(second.pass, true);
    assert.equal(second.cached, true, "a second call with nothing changed should be served from cache");
  });
});

test("check(): the cache is invalidated once the tree actually changes (a new uncommitted file)", async () => {
  await withTempDir(async (dir) => {
    await initGitRepo(dir);
    const checkCache = createCheckCache();
    const api = buildSemApi({ cwd: dir, semBin: "sem", checkCache });

    await api.check();
    writeFileSync(join(dir, "new-file.txt"), "changed");
    const afterChange = await api.check();
    assert.equal(afterChange.cached, undefined, "a real tree change must invalidate the cache, not reuse the stale result");
  });
});

test("check(): caching is SESSION-scoped -- shared across two separate buildSemApi() calls passing the same checkCache", async () => {
  await withTempDir(async (dir) => {
    await initGitRepo(dir);
    const checkCache = createCheckCache();

    const apiCall1 = buildSemApi({ cwd: dir, semBin: "sem", checkCache });
    await apiCall1.check();

    const apiCall2 = buildSemApi({ cwd: dir, semBin: "sem", checkCache });
    const result = await apiCall2.check();
    assert.equal(result.cached, true, "a check() from an earlier sem_code call should serve a later one's identical check() from cache");
  });
});

test("check(): outside a git repo, caching is disabled (always re-runs) rather than throwing", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(0)"` } }));
    const checkCache = createCheckCache();
    const api = buildSemApi({ cwd: dir, semBin: "sem", checkCache });
    const first = await api.check();
    const second = await api.check();
    assert.equal(first.pass, true);
    assert.equal(second.pass, true);
    assert.equal(second.cached, undefined, "no git repo means no tree fingerprint, so caching degrades to always-miss rather than erroring");
  });
});
