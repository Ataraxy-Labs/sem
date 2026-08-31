import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerSemCode, CODE_MODE_ADDENDUM, deriveApiCallStats } from "../../src/codemode/tool.ts";
import type { CallRecord } from "../../src/codemode/sandbox.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

const MAX_DESCRIPTION_LENGTH = 160;

type ToolExecute = (
  toolCallId: string,
  params: { code: string; timeout_ms?: number },
  signal: undefined,
  onUpdate: undefined,
  ctx: { cwd: string },
) => Promise<{ content: Array<{ type: string; text: string }>; details: Record<string, unknown> }>;

interface CapturedTool {
  name: string;
  description: string;
  parameters: { properties?: Record<string, unknown>; required?: string[] };
  execute?: ToolExecute;
}

type Handler = (event: unknown, ctx: unknown) => unknown;

function fakePi(): { pi: ExtensionAPI; captured: CapturedTool[]; handlers: Map<string, Handler> } {
  const captured: CapturedTool[] = [];
  const handlers = new Map<string, Handler>();
  const pi = {
    registerTool(def: CapturedTool) {
      captured.push({ name: def.name, description: def.description, parameters: def.parameters as CapturedTool["parameters"], execute: def.execute });
    },
    on(event: string, handler: Handler) {
      handlers.set(event, handler);
    },
  } as unknown as ExtensionAPI;
  return { pi, captured, handlers };
}

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-tool-execute-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

test("sem_code registers exactly one tool named sem_code, with a description under the 160-char cap", () => {
  const { pi, captured } = fakePi();
  registerSemCode(pi);
  assert.equal(captured.length, 1);
  const tool = captured[0]!;
  assert.equal(tool.name, "sem_code");
  assert.ok(
    tool.description.length <= MAX_DESCRIPTION_LENGTH,
    `sem_code's description is ${tool.description.length} chars, over the ${MAX_DESCRIPTION_LENGTH} cap: "${tool.description}"`,
  );
});

test("sem_code's params require `code` and accept an optional `timeout_ms`", () => {
  const { pi, captured } = fakePi();
  registerSemCode(pi);
  const tool = captured[0]!;
  assert.ok(tool.parameters.properties?.code, "params must declare `code`");
  assert.ok(tool.parameters.required?.includes("code"), "`code` must be required");
  assert.ok(tool.parameters.properties?.timeout_ms, "params must declare optional `timeout_ms`");
  assert.ok(!tool.parameters.required?.includes("timeout_ms"), "`timeout_ms` must be optional");
});

test("sem_code's params also accept an optional `budget` (v2 item 5: per-run token budget)", () => {
  const { pi, captured } = fakePi();
  registerSemCode(pi);
  const tool = captured[0]!;
  assert.ok(tool.parameters.properties?.budget, "params must declare optional `budget`");
  assert.ok(!tool.parameters.required?.includes("budget"), "`budget` must be optional");
});

test("before_agent_start prepends the addendum (v2 item 6: <=10 lines) and the sem-api.d.ts verbatim to the system prompt", async () => {
  const { pi, handlers } = fakePi();
  registerSemCode(pi);
  const handler = handlers.get("before_agent_start");
  assert.ok(handler, "sem_code must register a before_agent_start handler");

  const result = (await handler!({ systemPrompt: "BASE PROMPT" }, {})) as { systemPrompt: string } | undefined;
  assert.ok(result, "handler must return an updated systemPrompt");
  assert.match(result!.systemPrompt, /BASE PROMPT/);
  assert.match(result!.systemPrompt, /Code mode: call sem_code/);
  assert.match(result!.systemPrompt, /declare const sem/);
  const lineCount = CODE_MODE_ADDENDUM.split("\n").filter((l: string) => l.trim().length > 0).length;
  assert.ok(lineCount <= 10, `the addendum must stay <= 10 lines (pinned budget), got ${lineCount}`);
});

// v2 item 6: the 9-line prose addendum was replaced by a compact
// question -> verb table pointing at blast/why/where/explain/check/
// changed -- each of these tests checks one mapping the model needs to
// find, not the old prose's exact wording.
test("the addendum maps each question verb to its call", () => {
  assert.match(CODE_MODE_ADDENDUM, /"who's affected\?" -> sem\.blast/);
  assert.match(CODE_MODE_ADDENDUM, /"how are these connected\?" -> sem\.why/);
  assert.match(CODE_MODE_ADDENDUM, /"where does this live\?" -> sem\.where/);
  assert.match(CODE_MODE_ADDENDUM, /"what is this\?" -> sem\.explain/);
  assert.match(CODE_MODE_ADDENDUM, /"am I still green\?" -> sem\.check/);
  assert.match(CODE_MODE_ADDENDUM, /"what have I changed\?" -> sem\.changed/);
});

// Paired-eval finding: code mode won on all 8 tasks except that provider-
// native tools (apply_patch/exec_command) kept firing NEXT TO sem_code for
// the actual edit -- this line is the fix, deliberately preserved through
// the v2 rewrite rather than dropped to hit the line budget (see
// CODE_MODE_ADDENDUM's own doc comment).
test("the addendum steers all edits through sem.edit/sem.add, never a native apply_patch/exec_command/bash bypass", () => {
  assert.match(CODE_MODE_ADDENDUM, /never apply_patch\/patch\/exec_command\/bash redirection/);
  assert.match(CODE_MODE_ADDENDUM, /bypass verification and coordination/);
});

test("the addendum points at sem.more(handle) for a truncated result", () => {
  assert.match(CODE_MODE_ADDENDUM, /sem\.more\(handle\)/);
});

// Authority-on-failure finding (fix shape
// item 4): the model needs to be told plainly that an unawaited sem.*
// call is not confirmed by the time the script returns.
test("the addendum tells the model to await every sem.* call, since an unawaited one isn't confirmed", () => {
  assert.match(CODE_MODE_ADDENDUM, /await every sem\.\* call/i);
});

test("deriveApiCallStats builds a per-function histogram and edit-specific refusal reporting from sandbox.ts's generic call log", () => {
  const calls: CallRecord[] = [
    { fn: "grep", ok: true },
    { fn: "read", ok: true },
    { fn: "read", ok: true },
    { fn: "edit", ok: true },
    { fn: "edit", ok: false, error: "changes exported-ness" },
    { fn: "edit", ok: false, error: "no longer parses" },
  ];

  const stats = deriveApiCallStats(calls);

  assert.equal(stats.apiCalls.count, 6);
  assert.deepEqual(stats.apiCalls.histogram, { grep: 1, read: 2, edit: 3 });
  assert.equal(stats.edits.count, 3);
  assert.equal(stats.edits.refused, 2);
  assert.deepEqual(stats.edits.reasons, ["changes exported-ness", "no longer parses"]);
});

/**
 * "primitives before the first v2 verb" needs to be exact
 * WITHIN a single sem_code script, not just across separate sem_code
 * calls -- the histogram alone throws away order (three `grep` calls
 * and one before a `blast` collapse into the same {grep: 3, blast: 1}
 * whether the blast happened first, last, or in the middle). sandbox.ts's
 * CallRecord[] is already ordered; this only had to be surfaced.
 *
 * Settled contract (an independent verifier, which had guessed this
 * shape ahead of any real emission): top-level `apiCallSequence` (sibling to `apiCalls`, NOT
 * nested under it), plain lowercase strings, `:refused` suffix on a
 * failed call rather than upgrading every entry to an object shape.
 */
test("deriveApiCallStats.apiCallSequence preserves the exact call ORDER the histogram discards, marking refused calls", () => {
  const calls: CallRecord[] = [
    { fn: "grep", ok: true },
    { fn: "blast", ok: true },
    { fn: "grep", ok: true },
    { fn: "edit", ok: false, error: "no longer parses" },
    { fn: "check", ok: true },
  ];
  const stats = deriveApiCallStats(calls);
  assert.deepEqual(stats.apiCallSequence, ["grep", "blast", "grep", "edit:refused", "check"]);
  // the histogram alone can't tell you blast happened at index 1 (before
  // the second grep) -- apiCallSequence is what makes that visible.
  assert.deepEqual(stats.apiCalls.histogram, { grep: 2, blast: 1, edit: 1, check: 1 });
});

test("deriveApiCallStats reports zero edits, not undefined, when the script never called sem.edit", () => {
  const stats = deriveApiCallStats([{ fn: "grep", ok: true }]);
  assert.deepEqual(stats.edits, { count: 0, refused: 0, merged: 0, reasons: [] });
});

// End-to-end: prove the wiring (execute -> runInSandbox -> deriveApiCallStats
// -> details), not just the pure derivation function in isolation.
// Authority-on-failure finding (see test/codemode/
// authority-on-failure.test.ts): a script that never awaits a sem.* call
// (fire-and-forget) can return before that call settles -- the mutation
// still lands, invisibly, unless execute() surfaces it. This proves the
// tool-level fix end to end: a real unawaited sem.write() through the
// real (not fake) sandboxed sem API produces both a WARNING line in the
// tool's text AND details.outstandingCalls, not silence.
test("execute() surfaces an explicit WARNING and details.outstandingCalls when a script returns without awaiting a sem.* call", async () => {
  await withTempCopy(["unique.ts"], async (dir) => {
    const { pi, captured } = fakePi();
    // Opt-out surface: this test needs a live sem.write() to leave in
    // flight; under the pure default it would refuse synchronously and
    // there'd be nothing outstanding to account for.
    registerSemCode(pi, { pure: false });
    const execute = captured[0]!.execute!;

    const result = await execute(
      "call-1",
      { code: `sem.write("fire-and-forget.txt", "unawaited").then(() => {}); return "returned without awaiting the write";` },
      undefined,
      undefined,
      { cwd: dir },
    );

    assert.equal(result.details.ok, true, JSON.stringify(result.details));
    assert.equal(result.details.pendingAtResolve, 1, `expected exactly one pending call, got ${JSON.stringify(result.details)}`);
    const text = result.content[0]!.text;
    assert.match(text, /WARNING:.*1 sem\.\* call\(s\) were still in flight/i, `expected an explicit warning in the tool text, got: ${text}`);
    // item 6's next-call-literal rule: a warning the model can't act on
    // is only marginally better than silence -- must name a concrete
    // next call, not just state the fact.
    assert.match(text, /sem\.changed\(\)|sem\.check\(\)/, `expected the warning to tell the model what to DO next, got: ${text}`);

    // Let the outstanding write actually settle before withTempCopy's own
    // cleanup deletes `dir` out from under it -- otherwise the (real,
    // deliberately unawaited) write races the temp-dir teardown and fails
    // as an unhandled rejection after this test has already finished. The
    // race THIS test proves is "outstandingCalls reports it," not
    // "outlives the test process" -- draining it here is test hygiene,
    // not a change to what's being proven above.
    await new Promise((resolve) => setTimeout(resolve, 50));
  });
});

test("sem_code's execute result carries apiCalls and edits in details, derived from a real sandboxed run", async () => {
  await withTempCopy(["unique.ts"], async (dir) => {
    const { pi, captured } = fakePi();
    registerSemCode(pi);
    const execute = captured[0]!.execute!;

    const result = await execute(
      "call-1",
      { code: `await sem.read({ name: "onlyOne" }); await sem.grep("onlyOne"); return "done";` },
      undefined,
      undefined,
      { cwd: dir },
    );

    assert.equal(result.details.ok, true, JSON.stringify(result.details));
    const apiCalls = result.details.apiCalls as { count: number; histogram: Record<string, number> };
    const edits = result.details.edits as { count: number; refused: number; merged: number; reasons: string[] };
    assert.equal(apiCalls.count, 2);
    assert.deepEqual(apiCalls.histogram, { read: 1, grep: 1 });
    assert.deepEqual(edits, { count: 0, refused: 0, merged: 0, reasons: [] });
  });
});

// v2 item 5/6: the gap a slice-2 review caught -- a PER-RUN
// budget alone doesn't bind across separate sem_code calls, since a fresh
// budget is available again on the very next invocation. registerSemCode
// constructs ONE SessionBudget in its own outer closure (same pattern as
// handles/changes/checkCache/dedup), so cumulative spend and run count
// carry across separate execute() calls -- this proves that wiring
// end-to-end, not just SessionBudget's own unit behavior.
test("details.budget reports session_used/session_runs accumulating across SEPARATE execute() calls, plus a session summary line in the text", async () => {
  await withTempCopy(["unique.ts"], async (dir) => {
    const { pi, captured } = fakePi();
    registerSemCode(pi);
    const execute = captured[0]!.execute!;

    const first = await execute("call-1", { code: `await sem.find("onlyOne"); return "done";` }, undefined, undefined, { cwd: dir });
    const firstBudget = first.details.budget as { used: number; total: number; session_used: number; session_runs: number };
    assert.equal(firstBudget.session_runs, 1);
    assert.ok(firstBudget.session_used > 0);
    assert.match(first.content[0]!.text, /session: \d+k across 1 run\b/);

    const second = await execute("call-2", { code: `await sem.find("onlyOne"); return "done";` }, undefined, undefined, { cwd: dir });
    const secondBudget = second.details.budget as { used: number; total: number; session_used: number; session_runs: number };
    assert.equal(secondBudget.session_runs, 2, "a second sem_code call from the SAME registerSemCode() registration must accumulate, not reset");
    assert.ok(secondBudget.session_used > firstBudget.session_used, "session_used must grow across calls, not reset per-run like the per-run `used` does");
    assert.match(second.content[0]!.text, /session: \d+k across 2 runs\b/);
  });
});

// The batch-edit telemetry fidelity fix: previously a single script-level
// `sem.edit(request[])` call always counted as exactly ONE `edit` entry in
// apiCalls/edits regardless of how many entities were in the array or how
// many failed -- a real batch with 2 successes and 1 failure showed up here
// as `edits: {count:1, refused:0}`, hiding the failure from this telemetry
// entirely even though the script itself saw it inline. Now sandbox.ts's
// generic call log gets one CallRecord per entity (via api.ts's edit()
// attaching them to its return value through sandbox.ts's SUB_CALLS
// protocol), so this reflects the real per-entity outcome, not just
// "was sem.edit called at all".
test("sem_code's edits telemetry counts each entity in a batch sem.edit(request[]) call separately, not the whole call as one", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const { pi, captured } = fakePi();
    registerSemCode(pi);
    const execute = captured[0]!.execute!;

    const result = await execute(
      "call-1",
      {
        code: `
          const results = await sem.edit([
            { file: "calls.ts", entity: { name: "twice" }, op: "replace", content: "export function twice(n) {\\n  return add(n, n) + 0;\\n}" },
            { file: "calls.ts", entity: { name: "doesNotExist" }, op: "replace", content: "export function doesNotExist() {}" },
            { file: "calls.ts", entity: { name: "thrice" }, op: "replace", content: "export function thrice(n) {\\n  return add(n, n) + n + 0;\\n}" },
          ]);
          return results.length;
        `,
      },
      undefined,
      undefined,
      { cwd: dir },
    );

    assert.equal(result.details.ok, true, JSON.stringify(result.details));
    const apiCalls = result.details.apiCalls as { count: number; histogram: Record<string, number> };
    const edits = result.details.edits as { count: number; refused: number; merged: number; reasons: string[] };

    assert.equal(apiCalls.histogram.edit, 3, `expected 3 edit sub-calls recorded (one per entity), got: ${JSON.stringify(apiCalls)}`);
    assert.equal(edits.count, 3, `expected edits.count to reflect all 3 entities, got: ${JSON.stringify(edits)}`);
    assert.equal(edits.refused, 1, `expected exactly 1 refusal (doesNotExist), got: ${JSON.stringify(edits)}`);
    assert.equal(edits.reasons.length, 1);
    assert.match(edits.reasons[0]!, /no entity named "doesNotExist"/);
  });
});
