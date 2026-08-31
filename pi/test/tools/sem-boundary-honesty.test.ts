import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { fetchGraph, fetchHotspots, fetchCoChange, computePath } from "../../src/tools/internal/graph.ts";
import { performSemGraph } from "../../src/tools/sem-graph.ts";
import { performSemHotspots } from "../../src/tools/sem-hotspots.ts";
import { performSemCochange } from "../../src/tools/sem-cochange.ts";
import { extractEntities } from "../../src/tools/internal/entities.ts";
import { performSemFind } from "../../src/tools/sem-find.ts";
import { performSemCallers } from "../../src/tools/sem-callers.ts";
import { performSemGrep } from "../../src/tools/sem-grep.ts";

/**
 * The third "authority on failure" audit: the `sem` CLI/MCP
 * boundary as pi-sem consumes it. Found by accident this round:
 * `sem log <ambiguous> --json` -- misdiagnosed as environmental for three
 * sessions before someone noticed the CLI's refusal text was being thrown
 * away. Systematic sweep below, real `sem 0.23.1` binary (this machine's
 * PATH), read-only with respect to src/** -- findings + RED tests only,
 * fixes handed to the owning lane (this file lives under test/tools/,
 * mirroring where graph.ts/entities.ts/sem-find.ts/etc.'s own existing
 * tests already live, since the bug lives in shared internal/graph.ts,
 * not anything code-mode- or bridge-specific).
 *
 * CORRECTION to the reported repro: on this machine's real sem 0.23.1,
 * `sem log <ambiguous> --json` refuses cleanly -- exit 1, human message on
 * STDERR, stdout completely empty (verified directly: `sem log execute
 * --json 2>/dev/null` prints nothing; `... 1>/dev/null` prints the full
 * "Entity 'execute' found in multiple files..." refusal). It does NOT
 * exit 0, and the message is not on stdout. Either the originally-observed
 * build differed, or the "misdiagnosed as environmental" framing was about
 * a DIFFERENT call site's mishandling of this correctly-shaped stderr
 * refusal (see the fetchGraph/fetchHotspots/fetchCoChange finding below,
 * which is the exact bug this describes: a real refusal message discarded
 * behind a generic error). This distinction matters for the fix, so it's
 * recorded here rather than silently assumed to match the original report.
 */

const __dirname = dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = join(__dirname, "..", "..");
const FAKE_SEM_FAILS = join(__dirname, "fixtures", "fake-sem-command-fails.sh");

// ============================================================================
// FINDING (confirmed, high severity, wide blast radius): fetchGraph,
// fetchHotspots, and fetchCoChange (src/tools/internal/graph.ts) NEVER
// check exitCode at all -- they go straight from `runCommand` to
// `JSON.parse(result.stdout)`. When the real `sem` binary fails for ANY
// reason (bad/unsupported flag, corrupted index, an internal crash --
// verified concretely with a real flag-mismatch repro: `sem graph --json
// --no-default-excludes --this-flag-does-not-exist` exits 2, stdout
// EMPTY, a clear "unexpected argument ... Usage: sem graph ..." message
// on stderr), the real stderr text is NEVER LOOKED AT. The only message
// that reaches the caller is JSON.parse's own generic failure ("Unexpected
// end of JSON input"), which names neither the real cause nor a next
// action. This is WORSE than the sem-find.ts/sem-callers.ts/sem-grep.ts
// pattern (see the confirmed-fine findings below), which at least falls
// back to `result.stderr.trim()` in its catch block -- fetchGraph doesn't
// even try.
//
// Blast radius: fetchGraph backs sem_graph (native), sem_path (native, via
// computePath -> fetchGraph), sem.graph()/sem.path()/sem.blast()/sem.why()
// (code mode, same internal/graph.ts functions, confirmed via api.ts's own
// imports). fetchHotspots/fetchCoChange back sem_hotspots/sem_cochange
// (native) and sem.hotspots()/sem.cochange() (code mode) identically. ONE
// fix in internal/graph.ts closes it for both surfaces at once.
//
// What the MODEL actually sees today (not just an internal exception
// shape): performSemGraph/performSemHotspots/performSemCochange DO catch
// the thrown error and return {isError:true, text: "sem_graph: " +
// message, ...} -- so this is not a hang/crash/silence, structurally an
// error IS reported. The dishonesty is in the CONTENT: the model gets
// "sem_graph: <semBin> graph --json produced invalid JSON: Unexpected end
// of JSON input" -- not the real cause, not an actionable next step.
// ============================================================================

test("FIXED: fetchGraph surfaces the real sem failure reason (exitCode checked before parsing)", async () => {
  await assert.rejects(fetchGraph(FAKE_SEM_FAILS, REPO_ROOT), (err: Error) => {
    assert.match(err.message, /unexpected argument|Usage: sem graph/, "the real sem stderr text now reaches the caller");
    assert.doesNotMatch(err.message, /invalid JSON/, "the generic parse message is gone from the failure path");
    return true;
  });
});

test("FIXED: fetchHotspots surfaces the real sem failure reason the same way", async () => {
  await assert.rejects(fetchHotspots(FAKE_SEM_FAILS, REPO_ROOT, 20), (err: Error) => {
    assert.match(err.message, /unexpected argument|Usage: sem graph/);
    assert.doesNotMatch(err.message, /invalid JSON/);
    return true;
  });
});

test("FIXED: fetchCoChange surfaces the real sem failure reason the same way", async () => {
  await assert.rejects(fetchCoChange(FAKE_SEM_FAILS, REPO_ROOT, "someEntity", 20), (err: Error) => {
    assert.match(err.message, /unexpected argument|Usage: sem graph/);
    assert.doesNotMatch(err.message, /invalid JSON/);
    return true;
  });
});

test("FIXED: computePath (sem_path / sem.path()) inherits the fix via fetchGraph", async () => {
  await assert.rejects(computePath(FAKE_SEM_FAILS, REPO_ROOT, "a", "b", {}), (err: Error) => {
    assert.match(err.message, /unexpected argument|Usage: sem graph/);
    assert.doesNotMatch(err.message, /invalid JSON/);
    return true;
  });
});

test("FIXED, what the model actually sees: performSemGraph's reported text now names the real cause when sem fails", async () => {
  const outcome = await performSemGraph({ seed: "anything", hops: 1 }, { cwd: REPO_ROOT, semBin: FAKE_SEM_FAILS });
  assert.equal(outcome.isError, true);
  // This IS what a model reading sem_graph's result sees.
  assert.match(outcome.text, /unexpected argument|Usage: sem graph/, "the real, actionable sem error text reaches the model");
  assert.doesNotMatch(outcome.text, /invalid JSON/);
});

test("FIXED, what the model actually sees: performSemHotspots's reported text now carries the real sem failure", async () => {
  const outcome = await performSemHotspots({ limit: 5 }, { cwd: REPO_ROOT, semBin: FAKE_SEM_FAILS });
  assert.equal(outcome.isError, true);
  assert.match(outcome.text, /unexpected argument|Usage: sem graph/);
  assert.doesNotMatch(outcome.text, /invalid JSON/);
});

// ============================================================================
// Confirmed FINE (checked, not assumed): sem-find.ts / sem-callers.ts /
// sem-grep.ts's own catch blocks DO fall back to `result.stderr.trim()` on
// a JSON-parse failure, so a real sem error (bad flag, crash) DOES surface
// its actual text to the model through these three -- unlike fetchGraph
// et al above.
// ============================================================================

test("CONFIRMED FINE: performSemFind surfaces the real sem stderr text on a genuine sem failure, unlike fetchGraph", async () => {
  const outcome = await performSemFind({ query: "anything" }, { cwd: REPO_ROOT, semBin: FAKE_SEM_FAILS });
  assert.equal(outcome.isError, true);
  assert.match(outcome.text, /unexpected argument|Usage: sem graph/, "sem-find.ts's catch block DOES fall back to stderr, unlike fetchGraph's -- the real cause reaches the model here");
});

test("CONFIRMED FINE: performSemCallers surfaces the real sem stderr text on a genuine sem failure", async () => {
  const outcome = await performSemCallers({ name: "anything" }, { cwd: REPO_ROOT, semBin: FAKE_SEM_FAILS });
  assert.equal(outcome.isError, true);
  assert.match(outcome.text, /unexpected argument|Usage: sem graph/);
});

test("CONFIRMED FINE: performSemGrep surfaces the real sem stderr text on a genuine sem failure", async () => {
  const outcome = await performSemGrep({ pattern: "anything" }, { cwd: REPO_ROOT, semBin: FAKE_SEM_FAILS });
  assert.equal(outcome.isError, true);
  assert.match(outcome.text, /unexpected argument|Usage: sem graph/);
});

// ============================================================================
// Confirmed FINE (checked against the REAL sem binary, not a fake): for
// `find`/`callers`, an AMBIGUOUS name (multiple entities share it) is NOT a
// refusal at all -- sem just returns every match as a normal JSON array.
// Only `log`/`impact`/`context` treat ambiguity as a hard refusal
// (verified directly: `sem find execute --json` against this repo's own
// ~17-way-ambiguous "execute" name returns all 17 rows, exit 0; `sem log
// execute --json` refuses, exit 1, message on stderr). This means the
// "does ambiguity surface a candidate list or crash" question has TWO
// different correct answers depending on the subcommand, and pi-sem's own
// find/callers wrappers are already doing the right thing for their half
// (returning everything, letting a caller disambiguate) -- pinned here so
// a future regression toward "silently pick the first match" would be
// caught.
// ============================================================================

test("CONFIRMED FINE: sem find on a genuinely ambiguous name (this repo's own multiply-defined \"execute\") returns every match, never refuses or silently picks one", async () => {
  const outcome = await performSemFind({ query: "execute" }, { cwd: REPO_ROOT, semBin: "sem" });
  assert.equal(outcome.isError, false, outcome.text);
  const details = outcome.details as { hits?: Array<{ file: string }> };
  assert.ok((details.hits?.length ?? 0) >= 10, `expected the real multi-way ambiguous "execute" name to return every match as rows, got ${details.hits?.length}`);
});

test("CONFIRMED FINE, corrected from an initial wrong assumption: sem_callers (the HOST-SIDE wrapper, not the raw CLI) treats the same genuinely ambiguous name as a refusal, with a full, well-formed candidate list -- unlike raw `sem callers --json`, which just returns everything (see performSemCallers, sem-callers.ts: `if (groups.length > 1)` refuses). Both are honest; they just made opposite host-side UX choices on top of the same permissive CLI. Pinned so a regression toward silently picking one match would be caught.", async () => {
  const outcome = await performSemCallers({ name: "execute" }, { cwd: REPO_ROOT, semBin: "sem" });
  assert.equal(outcome.isError, true, "performSemCallers deliberately refuses ambiguity host-side, unlike sem_find");
  assert.match(outcome.text, /is ambiguous.*\d+ matches/i);
  assert.match(outcome.text, /Add entity_type to narrow it/);
});

// ============================================================================
// Confirmed FINE: entities.ts's extractEntities (sem_outline's own
// foundation) DOES check exitCode before parsing, and surfaces the real
// stderr text -- verified against a REAL sem failure (a nonexistent path),
// not just the fake fixture.
// ============================================================================

test("CONFIRMED FINE: extractEntities surfaces the real sem error (a nonexistent path) rather than a generic parse failure", async () => {
  await assert.rejects(extractEntities("sem", "/no/such/path/at/all-honesty-audit.ts", REPO_ROOT), (err: Error) => {
    assert.match(err.message, /Path not found/);
    return true;
  });
});

// ============================================================================
// Empty result vs genuine refusal vs "no matches": grep's own rg-derived
// convention (exit 1 with a valid `{hits:[], origin:"no_candidates"}` body)
// is honestly distinguishable from a real failure (which sem-grep.ts's own
// catch-and-fallback-to-stderr, confirmed above, still surfaces). Verified
// directly against the real binary for grep specifically -- find/callers
// already covered by the "returns everything" test above, and entities/
// context by the exitCode-checked tests above.
// ============================================================================

test("CONFIRMED FINE: sem grep's rg-convention exit 1 with zero real matches is a valid, honestly-empty result, not confused with a failure", async () => {
  // Built at runtime, never written as a literal anywhere -- a literal
  // "nonexistent" pattern string is itself now text IN this repo (this
  // file), which the first version of this test learned the hard way (it
  // matched its own source line).
  const { randomUUID } = await import("node:crypto");
  const guaranteedAbsentPattern = `ZZZ_${randomUUID().replace(/-/g, "")}_ZZZ`;
  const outcome = await performSemGrep({ pattern: guaranteedAbsentPattern }, { cwd: REPO_ROOT, semBin: "sem" });
  assert.equal(outcome.isError, false, outcome.text);
  const details = outcome.details as { hits?: unknown[] };
  assert.deepEqual(details.hits, []);
});

// ============================================================================
// Minor, non-blocking observation (not filed as a dishonesty finding): a
// real sem CLI error message can carry raw ANSI color escape codes (e.g.
// `sem diff HEAD~500 --json` against a repo without that many commits:
// `\x1b[31mError: git error: ...\x1b[0m` on stderr). runSemJson (codemode/
// api.ts, backs sem.diff()/sem.impact()/sem.history()) DOES capture this
// text faithfully -- the real cause reaches the model -- but with raw
// escape bytes still embedded, which would render as garbage in a
// text-only model context. Noted for whoever owns runSemJson to decide if
// stripping ANSI codes is worth adding; not a silence/dishonesty bug since
// the actual message content is still present and readable around the
// escape bytes.
// ============================================================================

test("OBSERVATION (minor, not a dishonesty finding): a real sem error can carry raw ANSI escape codes that reach whatever surfaces stderr verbatim", async () => {
  // A real, reproducible sem failure carrying ANSI codes: asking for a
  // diff further back than the repo's own history goes. Verified directly
  // before writing this assertion (not assumed): `sem diff HEAD~<beyond
  // history> --json` prints `\x1b[31mError: git error: ...\x1b[0m` on
  // stderr against this repo's own real history. A non-vacuous assertion
  // on purpose (this session's own earlier audit found the cost of
  // "checked and it's fine" tests that can't actually fail) -- if this repo
  // is ever shallow-cloned/rewritten such that the count-based repro stops
  // reproducing the ANSI codes, this test SHOULD fail loudly, not silently
  // pass having asserted nothing.
  const { execFileSync } = await import("node:child_process");
  const commitCount = Number(execFileSync("git", ["rev-list", "--count", "HEAD"], { cwd: REPO_ROOT, encoding: "utf8" }).trim());
  assert.ok(commitCount > 0, "this repo must have real history for this repro to mean anything");
  const { runCommand } = await import("../../src/tools/internal/proc.ts");
  const result = await runCommand("sem", ["diff", `HEAD~${commitCount + 500}`, "--json"], REPO_ROOT);
  assert.notEqual(result.exitCode, 0);
  // eslint-disable-next-line no-control-regex
  assert.match(result.stderr, /\x1b\[\d+m/, "confirms real sem error output can carry raw ANSI escapes -- documented for whoever owns the text-surfacing call sites (runSemJson et al)");
});
