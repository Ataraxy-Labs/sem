import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { registerSemCode, buildSystemPromptAddendum, CODE_MODE_ADDENDUM, CODE_MODE_MIXED_ADDENDUM } from "../../src/codemode/tool.ts";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

/**
 * MIXED MODE (PI_SEM_MODE=code + PI_SEM_PURE=0): sem_code AND the native
 * tools. CODE_MODE_ADDENDUM is written for the pure surface -- "only `sem`
 * is exposed", "never ... exec_command/bash redirection" -- so in mixed
 * mode it describes a tool set the model can see it does not have, and
 * nothing states the division of labor. CODE_MODE_MIXED_ADDENDUM states
 * it; this file pins the three properties that make that safe:
 *
 *   1. it renders in mixed mode and NOT in pure mode;
 *   2. pure mode's addendum is still exactly its pinned 10 lines,
 *      byte-identical to before the mixed block existed;
 *   3. the two blocks never contradict each other on the one rule they
 *      both carry -- every edit goes through sem.
 *
 * (3) is the load-bearing one. The pure block's edit-bypass line exists
 * because a paired eval measured provider-native apply_patch/exec_command
 * firing next to sem_code for the actual edit; mixed mode hands those
 * tools back, so a mixed block that softened the rule would reopen that
 * regression under the banner of "division of labor".
 */

type Handler = (event: unknown, ctx: unknown) => unknown;

function fakePi(): { pi: ExtensionAPI; handlers: Map<string, Handler> } {
  const handlers = new Map<string, Handler>();
  const pi = {
    registerTool() {},
    on(event: string, handler: Handler) {
      handlers.set(event, handler);
    },
  } as unknown as ExtensionAPI;
  return { pi, handlers };
}

function emptyRepo(): string {
  return mkdtempSync(join(tmpdir(), "sem-mixed-prompt-"));
}

// (1) -- mode conditionality, at the function level and end to end.

test("mixed mode (pure=false) renders the division-of-labor block; pure mode does not", () => {
  const dir = emptyRepo();
  try {
    const mixed = buildSystemPromptAddendum("table", dir, undefined, false);
    const pure = buildSystemPromptAddendum("table", dir, undefined, true);

    assert.ok(mixed.includes(CODE_MODE_MIXED_ADDENDUM), "mixed mode must carry the mixed block verbatim");
    assert.ok(!pure.includes(CODE_MODE_MIXED_ADDENDUM), "pure mode must not carry the mixed block");
    assert.ok(!pure.includes("Mixed mode:"), "pure mode must not carry any trace of the mixed block");
    assert.ok(mixed.startsWith(CODE_MODE_ADDENDUM), "the pinned block stays first; the mixed block follows it");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the mixed block rides every prompt shape, and only in mixed mode", () => {
  const dir = emptyRepo();
  try {
    for (const shape of ["table", "dts", "recipes"] as const) {
      assert.ok(buildSystemPromptAddendum(shape, dir, undefined, false).includes(CODE_MODE_MIXED_ADDENDUM), `shape ${shape} must carry the mixed block in mixed mode`);
      assert.ok(!buildSystemPromptAddendum(shape, dir, undefined, true).includes(CODE_MODE_MIXED_ADDENDUM), `shape ${shape} must not carry it in pure mode`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("registerSemCode({pure:false}) injects the mixed block into the real before_agent_start system prompt; {pure:true} does not", async () => {
  for (const pure of [false, true]) {
    const { pi, handlers } = fakePi();
    registerSemCode(pi, { pure });
    const handler = handlers.get("before_agent_start");
    assert.ok(handler, "sem_code must register a before_agent_start handler");
    const result = (await handler!({ systemPrompt: "BASE PROMPT" }, {})) as { systemPrompt: string };
    assert.match(result.systemPrompt, /BASE PROMPT/);
    assert.match(result.systemPrompt, /Code mode: call sem_code/, "the pinned block is present in both modes");
    assert.equal(result.systemPrompt.includes(CODE_MODE_MIXED_ADDENDUM), !pure, `pure=${pure} must ${pure ? "not " : ""}render the mixed block`);
  }
});

// (2) -- the pure-mode pin, restated here so a change to the mixed block
// can never silently move the pure one.

test("pure mode's addendum is STILL exactly its pinned 10 lines, byte-identical to CODE_MODE_ADDENDUM", () => {
  const dir = emptyRepo();
  try {
    assert.equal(buildSystemPromptAddendum("table", dir, undefined, true), CODE_MODE_ADDENDUM, "pure mode renders the pinned block and nothing else");
    const lines = CODE_MODE_ADDENDUM.split("\n").filter((l) => l.trim().length > 0);
    assert.equal(lines.length, 10, `the pure addendum's pin is exactly 10 lines, got ${lines.length}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the mixed block earns its prompt budget: <= 8 lines (P8 -- an unused prompt line is dead weight paid for every turn)", () => {
  const lines = CODE_MODE_MIXED_ADDENDUM.split("\n").filter((l) => l.trim().length > 0);
  assert.ok(lines.length <= 8, `the mixed block must stay <= 8 lines, got ${lines.length}`);
});

// (3) -- the no-contradiction pin.

test("the mixed block never contradicts the pure block's edit rule: BOTH route every edit through sem", () => {
  // The pure block's rule.
  assert.match(CODE_MODE_ADDENDUM, /All edits go through sem\.edit\/sem\.add/);
  assert.match(CODE_MODE_ADDENDUM, /never apply_patch\/patch\/exec_command\/bash redirection/);

  // The mixed block restates the SAME rule for the surface that now has
  // those tools back -- it never carves out an exception for them.
  assert.match(CODE_MODE_MIXED_ADDENDUM, /every edit \(sem\.edit \/ sem\.add \/ sem\.rename\)/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Never cat\/sed\/grep\/echo>\/apply_patch or the native edit tool on a source file/);

  // No line may permit a native edit of source. Any sentence that mentions
  // the native edit path must be a prohibition, never a permission.
  for (const sentence of CODE_MODE_MIXED_ADDENDUM.split(/(?<=[.:;])\s+/)) {
    if (!/apply_patch|native edit|sed |cat /.test(sentence)) continue;
    assert.match(sentence, /Never|never|bypass/, `a sentence naming the native edit path must forbid it, got: ${sentence}`);
  }

  // The one sanctioned fallback is explicitly scoped to what sem cannot
  // address at all -- and must be declared, not taken silently.
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Fall back only for what is genuinely outside sem's domain/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /and say which/);
});

test("the mixed block scopes bash to EXECUTION -- the capabilities the vanilla arm actually used, not reading or editing", () => {
  assert.match(CODE_MODE_MIXED_ADDENDUM, /bash is for EXECUTION, not for reading or editing code/);
  // repro-before-edit 9/12, real test runs 6/12, env provisioning 12/12,
  // read-only git archaeology 10/12 (decisive 2/12).
  assert.match(CODE_MODE_MIXED_ADDENDUM, /reproduce the bug before you edit it/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /run the project's real tests after you edit/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /install or provision whatever running requires/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /read-only git forensics \(git log -S, git blame\)/);
});

test("the mixed block carries the reproduce -> edit -> verify habit and the honest-null clause", () => {
  // django-15368: the gold line was produced and then OVERWRITTEN for want
  // of a red/green signal. matplotlib-24970 (vanilla): shipped a fix its
  // own check still showed failing, and reported success anyway.
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Reproduce -> edit via sem -> verify by running/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Never trust a change you have not executed/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /say so plainly instead of implying it was verified/);
});

test("the mixed block keeps the solve-from-the-repo rule pure mode gets by construction", () => {
  // 210/325 vanilla runs answer_adjacent; django-15252 fetched the upstream
  // diff and discarded its own already-passing fix. Mixed mode has a
  // network; the prompt is the only thing that can ask.
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Never fetch an answer from outside the repo/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Solve it from this repo/);
});

test("the mixed block keeps the one-script idiom -- the one addendum line measured as working", () => {
  assert.match(CODE_MODE_MIXED_ADDENDUM, /Keep the one-script idiom for sem work/);
  // The same-file concurrency affordance needs no restatement: the pure
  // block carries it unchanged and is rendered in BOTH modes.
  assert.match(CODE_MODE_ADDENDUM, /Concurrent edits to the SAME file are safe/);
});

test("the mixed block points failures back at the CALL, not at a bash fallback for the same operation", () => {
  assert.match(CODE_MODE_MIXED_ADDENDUM, /fix the CALL -- do not retry the same operation in bash/);
  // The three fixable-call shapes the study measured most: grep's regex
  // parse errors (36% of runs), entity-resolution disambiguators (43
  // self-contradictory not-founds), and check()'s env/runner story.
  assert.match(CODE_MODE_MIXED_ADDENDUM, /\{literal:true\}/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /parent_name\/entity_type/);
  assert.match(CODE_MODE_MIXED_ADDENDUM, /\.sem\/check\.json or check\(\{env\}\)/);
});

// The anti-benchmaxxing pin: the study rules out any prompt line that
// only pays off on one task shape, so the block must name no repo, no
// framework, no test runner, and no benchmark.
test("the mixed block is task-shape-free -- no benchmark, framework, or runner names", () => {
  for (const forbidden of ["swe-bench", "swebench", "django", "pytest", "astropy", "matplotlib", "xarray", "conftest", "runtests.py"]) {
    assert.ok(!CODE_MODE_MIXED_ADDENDUM.toLowerCase().includes(forbidden), `the mixed block must not name ${forbidden}`);
  }
});
