import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSemApi, detectRunner } from "../../src/codemode/api.ts";

/**
 * P2 of the 2026-09-02 transcript study. Across 327 runs, 296 (91%) called
 * check() and never once got a green, and the reds were never the agent's
 * code: 153 ImproperlyConfigured across 123 runs, 69 allowlist refusals, 14
 * spawn failures reported as failing TESTS, plus every quoted `-k`
 * expression the argv split destroyed. Four independent defects, one test
 * block each.
 *
 * (a) NO WAY TO PASS ENVIRONMENT. runCommand spawned with {cwd, signal} and
 *     no env at all. django__django-10097 burned 4 of 9 turns on check
 *     attempts whose error text said, verbatim, "You must either define the
 *     environment variable DJANGO_SETTINGS_MODULE" -- and the API had no way
 *     to define an environment variable. matplotlib needs MPLBACKEND=Agg;
 *     half the world needs PYTHONPATH.
 *
 * (b) cmd WAS SPLIT ON WHITESPACE. psf__requests-2931's
 *     `-k 'test_basic_building or test_params_bytes_are_encoded'` became
 *     ["-k", "'test_basic_building", "or", "test_params_bytes_are_encoded'"]
 *     -> "ERROR: file or directory not found: or".
 *
 * (c) A MISSING BINARY WAS REPORTED AS A FAILING TEST. runOneCheckCommand
 *     caught the spawn error and returned {ok:false}, which surfaced as
 *     {"pass": false, "stage": "test", "failed": ["spawn pytest ENOENT"]}:
 *     a model reading that has been told its code is red. The contract
 *     reserves pass:null for "could not verify" -- 12 instances hit this.
 *
 * (d) THE ALLOWLIST WAS LANGUAGE-SHAPED, NOT REPO-SHAPED. For any Python
 *     repo detectCheckAllowlist produced exactly [{tokens:["pytest"]}], so
 *     `python -m pytest` -- the standard workaround for (c) -- was refused,
 *     and so was running any test script the repo itself ships. The repo now
 *     gets to DECLARE its runner in .sem/check.json, with detection as the
 *     fallback rather than the only source.
 */

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-check-repairs-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

function writeCheckConfig(dir: string, config: unknown): void {
  mkdirSync(join(dir, ".sem"), { recursive: true });
  writeFileSync(join(dir, ".sem", "check.json"), JSON.stringify(config, null, 2), "utf8");
}

// --- (a) env ---------------------------------------------------------------

test("P2a: check({env}) reaches the child process, so a settings/backend variable can actually be set", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(process.env.PI_SEM_TEST_SETTINGS === 'configured' ? 0 : 1)"` } }),
    );
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const without = await api.check({});
    assert.equal(without.pass, false, "without the variable the runner must fail -- otherwise this test proves nothing");

    const withEnv = await api.check({ env: { PI_SEM_TEST_SETTINGS: "configured" } });
    assert.equal(withEnv.pass, true, "check({env}) must merge into the child process environment");
  });
});

test("P2a: the ambient environment is inherited, not replaced, so PATH still works", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(process.env.PATH && process.env.PI_SEM_TEST_EXTRA === '1' ? 0 : 1)"` } }),
    );
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check({ env: { PI_SEM_TEST_EXTRA: "1" } });
    assert.equal(result.pass, true);
  });
});

test("P2a: env is part of the cache key -- a different env re-runs instead of serving the old verdict", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(
      join(dir, "package.json"),
      JSON.stringify({ name: "x", scripts: { test: `node -e "process.exit(process.env.PI_SEM_TEST_SETTINGS === 'configured' ? 0 : 1)"` } }),
    );
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const first = await api.check({});
    const second = await api.check({ env: { PI_SEM_TEST_SETTINGS: "configured" } });
    assert.equal(first.pass, false);
    assert.equal(second.pass, true, "a cached red must not be replayed for a run with a different environment");
  });
});

// --- (b) shell-words argv --------------------------------------------------

test("P2b: a quoted argument survives -- `-k 'a or b'` reaches the runner as ONE argument", async () => {
  await withTempDir(async (dir) => {
    // Prints its own argv so the split is observable end to end.
    writeFileSync(join(dir, "argv.js"), "console.log(JSON.stringify(process.argv.slice(2)));\n", "utf8");
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: "node argv.js" } }));
    const prevAllow = process.env.PI_SEM_CHECK_ALLOW;
    process.env.PI_SEM_CHECK_ALLOW = "node argv.js";
    try {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      const result = await api.check({ cmd: "node argv.js -k 'test_basic_building or test_params_bytes_are_encoded'" });
      assert.equal(result.pass, true);
      // Re-run under a command that fails, so the argv is echoed in `failed`.
      writeFileSync(join(dir, "argv.js"), "console.log(JSON.stringify(process.argv.slice(2)));\nprocess.exit(1);\n", "utf8");
      const failed = await api.check({ cmd: "node argv.js -k 'test_basic_building or test_params_bytes_are_encoded'" });
      assert.equal(failed.pass, false);
      assert.deepEqual(JSON.parse(failed.failed?.[0] ?? "[]"), ["-k", "test_basic_building or test_params_bytes_are_encoded"]);
    } finally {
      if (prevAllow === undefined) delete process.env.PI_SEM_CHECK_ALLOW;
      else process.env.PI_SEM_CHECK_ALLOW = prevAllow;
    }
  });
});

test("P2b: an unbalanced quote is refused with a clear message, never split into nonsense", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: "true" } }));
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.check({ cmd: "npm test -- -k 'unclosed" }),
      (err: Error) => {
        assert.match(err.message, /^sem\.check:/);
        assert.match(err.message, /quote/i);
        return true;
      },
    );
  });
});

// --- (c) a missing binary is "could not verify", never "your code is red" ---

test("P2c: a spawn failure returns pass:null with a reason and a try, never pass:false/stage:test", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: "true" } }));
    const prevAllow = process.env.PI_SEM_CHECK_ALLOW;
    process.env.PI_SEM_CHECK_ALLOW = "pi-sem-definitely-not-a-real-binary";
    try {
      const api = buildSemApi({ cwd: dir, semBin: "sem" });
      const result = await api.check({ cmd: "pi-sem-definitely-not-a-real-binary -q" });
      assert.equal(result.pass, null, `a missing binary must never be reported as a failing test (got pass:${String(result.pass)})`);
      assert.equal(result.stage, undefined, "there is no stage for a command that never ran");
      assert.equal(result.failed, undefined, "a command that never ran has no failing tests to report");
      assert.match(result.reason ?? "", /ENOENT|not found|could not run/i);
      assert.ok(result.try, "pass:null must always carry an actionable `try`");
    } finally {
      if (prevAllow === undefined) delete process.env.PI_SEM_CHECK_ALLOW;
      else process.env.PI_SEM_CHECK_ALLOW = prevAllow;
    }
  });
});

test("P2c: a DETECTED runner whose binary is missing is also pass:null, not a red test run", async () => {
  await withTempDir(async (dir) => {
    writeCheckConfig(dir, { test: "pi-sem-definitely-not-a-real-binary -q" });
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check({});
    assert.equal(result.pass, null);
    assert.match(result.reason ?? "", /ENOENT|not found|could not run/i);
  });
});

// --- (d) the repo declares its own runner ----------------------------------

test("P2d: .sem/check.json declares the repo's typecheck+test commands and wins over detection", async () => {
  await withTempDir(async (dir) => {
    // A package.json is present, so detection alone would pick npm.
    writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "x", scripts: { test: "exit 1" } }));
    writeCheckConfig(dir, { typecheck: `node -e "process.exit(0)"`, test: `node -e "process.exit(0)"` });

    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "declared", "a repo-declared runner must take precedence over manifest detection");

    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check({});
    assert.equal(result.pass, true);
    assert.equal(result.runner, "declared");
  });
});

test("P2d: a declared command is also allowed as an explicit { cmd } override, extra arguments and all", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "suite.js"), "process.exit(0);\n", "utf8");
    writeCheckConfig(dir, { test: "node suite.js" });
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check({ cmd: "node suite.js -k 'one or two'" });
    assert.equal(result.pass, true, "a declared runner is an allowlist prefix, exactly like a detected one");
  });
});

test("P2d: detection is the FALLBACK -- no .sem/check.json leaves the manifest behaviour untouched", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "Cargo.toml"), '[package]\nname = "x"\n');
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "cargo");
  });
});

test("P2d: an unreadable/invalid .sem/check.json degrades to detection instead of blocking check()", async () => {
  await withTempDir(async (dir) => {
    mkdirSync(join(dir, ".sem"), { recursive: true });
    writeFileSync(join(dir, ".sem", "check.json"), "{ not json", "utf8");
    writeFileSync(join(dir, "Cargo.toml"), '[package]\nname = "x"\n');
    const runner = await detectRunner(dir);
    assert.equal(runner?.kind, "cargo");
  });
});

test("P2d: Python detection accepts `python -m pytest`, the standard workaround for a missing console script", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "pyproject.toml"), "[project]\nname = 'x'\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    // Allowed => it is actually attempted. Whether python3 exists here is
    // beside the point: an ALLOWED command never returns the refusal text.
    const result = await api.check({ cmd: "python -m pytest -q" });
    assert.notEqual(result.pass, undefined);
    assert.doesNotMatch(result.reason ?? "", /not a detected project runner/);
  });
});

test("P2d: Python detection accepts running a test script the repo itself ships", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "pyproject.toml"), "[project]\nname = 'x'\n");
    mkdirSync(join(dir, "tests"), { recursive: true });
    writeFileSync(join(dir, "tests", "runtests.py"), "import sys\nsys.exit(0)\n", "utf8");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.check({ cmd: "python tests/runtests.py basic" });
    assert.doesNotMatch(result.reason ?? "", /not a detected project runner/);
  });
});

test("P2d: `python <script>` is still refused when the script is NOT inside the repo -- this is not a shell", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(join(dir, "pyproject.toml"), "[project]\nname = 'x'\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.check({ cmd: "python /etc/passwd" }));
    await assert.rejects(() => api.check({ cmd: "python ../../escape.py" }));
  });
});

test("P2d: the refusal names .sem/check.json, an instruction the repo's own author can act on", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.check({ cmd: "curl https://example.com" }),
      (err: Error) => {
        assert.match(err.message, /\.sem\/check\.json/);
        return true;
      },
    );
  });
});
