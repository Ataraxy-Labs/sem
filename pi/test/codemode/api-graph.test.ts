import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";
import { runCommand } from "../../src/tools/internal/proc.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-graph-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

// chain.ts: alpha -> beta -> gamma -> delta (no cycle, deterministic shortest path)
// loop.ts: loopA -> loopB -> loopC -> loopA (a 3-cycle, isolated from chain.ts)

test("graph() direction:'out' from beta finds gamma (what beta calls) but not alpha (who calls beta)", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.graph({ name: "beta" }, { direction: "out", hops: 1 })) as {
      nodes: Array<{ name: string }>;
      edges: Array<{ from: string; to: string; kind: string }>;
      truncated: boolean;
    };
    const names = result.nodes.map((n) => n.name);
    assert.ok(names.includes("beta"));
    assert.ok(names.includes("gamma"), `expected gamma (beta's callee) in ${JSON.stringify(names)}`);
    assert.ok(!names.includes("alpha"), `alpha (beta's caller) should not appear in 'out' direction: ${JSON.stringify(names)}`);
    assert.equal(result.truncated, false);
  });
});

test("graph() direction:'in' from beta finds alpha (who calls beta) but not gamma (what beta calls)", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.graph({ name: "beta" }, { direction: "in", hops: 1 })) as { nodes: Array<{ name: string }> };
    const names = result.nodes.map((n) => n.name);
    assert.ok(names.includes("alpha"));
    assert.ok(!names.includes("gamma"), `gamma should not appear in 'in' direction: ${JSON.stringify(names)}`);
  });
});

test("graph() direction:'both' with enough hops reaches the whole chain from either end", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.graph({ name: "alpha" }, { direction: "both", hops: 3 })) as { nodes: Array<{ name: string }> };
    assert.deepEqual(
      result.nodes.map((n) => n.name).sort(),
      ["alpha", "beta", "delta", "gamma"],
    );
  });
});

test("graph() accepts multiple seeds in one call", async () => {
  await withTempCopy(["chain.ts", "loop.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.graph([{ name: "alpha" }, { name: "loopA" }], { hops: 1 })) as { nodes: Array<{ name: string }> };
    const names = result.nodes.map((n) => n.name).sort();
    assert.ok(names.includes("alpha") && names.includes("beta"));
    assert.ok(names.includes("loopA") && (names.includes("loopB") || names.includes("loopC")));
  });
});

test("graph() throws with candidate files when the seed name is ambiguous", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(() => api.graph({ name: "widget" }, {}), /ambiguous/i);
  });
});

test("graph() over a cycle (loop.ts) terminates and finds every node, truncated: false", async () => {
  await withTempCopy(["loop.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.graph({ name: "loopA" }, { hops: 5 })) as { nodes: Array<{ name: string }>; truncated: boolean };
    assert.deepEqual(
      result.nodes.map((n) => n.name).sort(),
      ["loopA", "loopB", "loopC"],
    );
    assert.equal(result.truncated, false);
  });
});

test("path() finds the exact ordered chain from alpha to delta", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.path({ name: "alpha" }, { name: "delta" }, {})) as Array<{ name: string }>;
    assert.deepEqual(
      result.map((n) => n.name),
      ["alpha", "beta", "gamma", "delta"],
    );
  });
});

test("path() over a cycle terminates and finds a real path rather than hanging", async () => {
  await withTempCopy(["loop.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await api.path({ name: "loopA" }, { name: "loopC" }, {})) as Array<{ name: string }> | null;
    assert.ok(result, "expected a path to be found, not null");
    assert.ok(result!.length >= 2);
    assert.equal(result![0]!.name, "loopA");
    assert.equal(result![result!.length - 1]!.name, "loopC");
  });
});

test("path() returns null when there is genuinely no connection between two entities", async () => {
  await withTempCopy(["chain.ts", "loop.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.path({ name: "alpha" }, { name: "loopA" }, {});
    assert.equal(result, null);
  });
});

test("path() respects max_hops, returning null when the real path is longer than the bound", async () => {
  await withTempCopy(["chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.path({ name: "alpha" }, { name: "delta" }, { max_hops: 1 });
    assert.equal(result, null, "alpha to delta is 3 hops apart -- max_hops:1 must not find it");
  });
});

/**
 * Real bug found via a flaky-looking failure in this suite's own live-repo
 * hotspots/cochange/history test below, once this repo's own commit
 * history made an ambiguous name (this codebase's own tool-registration
 * pattern gives ~15 files their own `execute` method) the top hotspot.
 * Root-caused via direct reproduction (`sem log <ambiguous-name> --json`):
 * the CLI exits non-zero with a CLEAR "found in multiple files ... Use
 * --file to disambiguate" message on STDERR, empty STDOUT -- history()'s
 * PREVIOUS code unconditionally `JSON.parse(result.stdout)`'d without
 * checking `exitCode` first, discarding that real message for a confusing
 * "invalid JSON: Unexpected end of JSON input" crash instead. This test
 * reproduces it deterministically (dup-a.ts/dup-b.ts BOTH define `widget`,
 * regardless of what today's live top hotspot happens to be), rather than
 * relying on the live-repo test below to catch it again by chance.
 */
/**
 * team-lead independently reproduced this repo's own ambiguous-entity bug
 * against a DIFFERENT sem build that signals the refusal via exit 0 with
 * the human-readable candidate list on STDOUT -- not the exit-1 + STDERR
 * shape this machine's installed sem 0.23.1 actually produces (confirmed
 * via direct reproduction, see runSemJson's doc comment in api.ts). Since
 * which shape a given `sem` build uses is downstream-version-dependent,
 * runSemJson handles both defensively; this test proves the exit-0 path
 * specifically and deterministically, via a fake `sem` binary stub
 * (fixtures/fake-sem-ambiguous-exit0.mjs) rather than depending on any
 * particular installed sem version behaving one way or the other.
 */
test("runSemJson (via history()) surfaces the CLI's refusal even when it's signaled as exit 0 with non-JSON text on stdout", async () => {
  const fakeSem = join(FIXTURES, "fake-sem-ambiguous-exit0.mjs");
  const api = buildSemApi({ cwd: __dirname, semBin: fakeSem });
  await assert.rejects(
    () => api.history("widget"),
    (err: Error) => {
      assert.match(err.message, /found in multiple files/i, `expected the stubbed CLI's ambiguity message, got: ${err.message}`);
      assert.match(err.message, /a\.ts/, `expected the candidate list to survive, got: ${err.message}`);
      assert.match(err.message, /b\.ts/, `expected the candidate list to survive, got: ${err.message}`);
      assert.match(err.message, /sem\.where/, `item 6: an ambiguity refusal should point at the disambiguating next call, got: ${err.message}`);
      assert.doesNotMatch(err.message, /invalid JSON|Unexpected/i, `must not fall back to a generic parse-error message: ${err.message}`);
      return true;
    },
  );
});

/**
 * impact() has its OWN resolveOneFile()-based --file disambiguation, so
 * dup-a.ts/dup-b.ts's cross-FILE ambiguity (one `widget` per file) can't
 * exercise this: resolveOneFile picks the first file and impact()
 * succeeds cleanly against it. The real failure mode (confirmed
 * empirically against this repo's own `execute` -- extensions/pi-sem.ts
 * defines it TWICE) needs two same-named entities in the SAME file, so
 * even after --file narrows to one candidate file, sem impact still
 * refuses ambiguity within it. same-file-dup.ts reproduces that shape
 * deterministically.
 */
test("impact() refuses an ambiguous entity name (two entities, same file) with the real CLI message, not a JSON-parse crash", async () => {
  await withTempCopy(["same-file-dup.ts"], async (dir) => {
    await runCommand("git", ["init", "-q"], dir);
    await runCommand("git", ["config", "user.email", "test@example.com"], dir);
    await runCommand("git", ["config", "user.name", "Test"], dir);
    await runCommand("git", ["add", "."], dir);
    await runCommand("git", ["commit", "-q", "-m", "init"], dir);

    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.impact("widget"),
      (err: Error) => {
        assert.match(err.message, /ambiguous/i, `expected the real CLI ambiguity message, got: ${err.message}`);
        assert.doesNotMatch(err.message, /Unexpected end of JSON input/, `must not be the old confusing JSON-parse crash message: ${err.message}`);
        return true;
      },
    );
  });
});

test("history() refuses an ambiguous entity name with the real CLI message, not a JSON-parse crash", async () => {
  await withTempCopy(["dup-a.ts", "dup-b.ts"], async (dir) => {
    await runCommand("git", ["init", "-q"], dir);
    await runCommand("git", ["config", "user.email", "test@example.com"], dir);
    await runCommand("git", ["config", "user.name", "Test"], dir);
    await runCommand("git", ["add", "."], dir);
    await runCommand("git", ["commit", "-q", "-m", "init"], dir);

    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await assert.rejects(
      () => api.history("widget"),
      (err: Error) => {
        assert.match(err.message, /found in multiple files/i, `expected the real CLI ambiguity message, got: ${err.message}`);
        assert.doesNotMatch(err.message, /Unexpected end of JSON input/, `must not be the old confusing JSON-parse crash message: ${err.message}`);
        return true;
      },
    );
  });
});

test("hotspots()/cochange()/history() run against this repo's own real git history and return well-shaped results", async () => {
  const cwd = join(__dirname, "..", "..");
  const api = buildSemApi({ cwd, semBin: "sem" });

  const hot = (await api.hotspots({ limit: 5 })) as Array<{ entity: string; file: string; commits: number }>;
  assert.ok(Array.isArray(hot));
  assert.ok(hot.length <= 5);
  if (hot.length > 0) {
    assert.ok(hot[0]!.entity);
    assert.ok(hot[0]!.file);
    assert.ok(typeof hot[0]!.commits === "number");
  }

  const targetEntity = hot[0]?.entity;
  if (targetEntity) {
    const pairs = (await api.cochange(targetEntity, { limit: 5 })) as Array<{ a: { entity: string }; b: { entity: string } }>;
    assert.ok(Array.isArray(pairs));
    for (const p of pairs) assert.ok(p.a.entity === targetEntity || p.b.entity === targetEntity);

    // The top hotspot in THIS repo's own live, growing git history can
    // legitimately be a name that either (a) exists in more than one file
    // (e.g. "execute", a common tool-registration method name repeated
    // across many files) -- history() then correctly refuses with a clear
    // "found in multiple files ..." message rather than silently
    // resolving one specific match -- or (b) is a historical-only entity
    // that no longer exists at HEAD (renamed, refactored away, or deleted
    // since the commits `hotspots()` counted it in) -- history() then
    // correctly refuses with "not found in the working tree ...", naming
    // `--file` as the way to search historical entities. A bare try/catch
    // here would mask WHICH branch a run actually exercised, so resolve
    // the branch EXPLICITLY and assert against that branch.
    //
    // The signal must be `sem log` ITSELF, not a proxy: this test
    // previously derived uniqueness from `sem find`'s file set, which
    // diverges -- sem log's auto-detection resolves against a narrower
    // scope that excludes test files. Probing the CLI directly keeps this
    // test faithful to the behavior history() actually wraps, and immune
    // to drift in sem's resolution scope.
    const logProbe = await runCommand("sem", ["log", targetEntity, "--json", "--limit", "1"], cwd);
    const cliRefusedAmbiguous = logProbe.exitCode !== 0 && /found in multiple files/i.test(logProbe.stderr);
    const cliRefusedHistorical = logProbe.exitCode !== 0 && !cliRefusedAmbiguous && /not found in the working tree/i.test(logProbe.stderr);
    if (logProbe.exitCode !== 0 && !cliRefusedAmbiguous && !cliRefusedHistorical) {
      assert.fail(`sem log "${targetEntity}" failed for an unexpected reason: ${logProbe.stderr}`);
    }

    if (!cliRefusedAmbiguous && !cliRefusedHistorical) {
      const hist = (await api.history(targetEntity, { limit: 5 })) as { entity: string; changes: unknown[] };
      assert.equal(hist.entity, targetEntity);
      assert.ok(Array.isArray(hist.changes));
    } else {
      await assert.rejects(
        () => api.history(targetEntity, { limit: 5 }),
        (err: Error) => {
          assert.match(
            err.message,
            /found in multiple files|ambiguous|not found in the working tree/i,
            `history("${targetEntity}") failed for an unexpected reason: ${err.message}`,
          );
          return true;
        },
      );
    }
  }
});
