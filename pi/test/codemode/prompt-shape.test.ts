import { test } from "node:test";
import assert from "node:assert/strict";
import { registerSemCode, buildSystemPromptAddendum, resolvePromptShape, CODE_MODE_ADDENDUM, CODE_MODE_RECIPES } from "../../src/codemode/tool.ts";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

/**
 * PI_SEM_PROMPT dimension (team-lead's ask): three env-selectable system-
 * prompt shapes so a gate can MEASURE which one wins on tokens and
 * correctness, rather than guess. This file exercises the shape-building
 * logic in isolation (buildSystemPromptAddendum(shape) takes an explicit
 * shape, so these tests don't need to mutate process.env) plus one
 * end-to-end check that PI_SEM_PROMPT is actually read by
 * registerSemCode's before_agent_start handler.
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

test("resolvePromptShape() defaults to \"recipes\" when PI_SEM_PROMPT is unset -- the measured-stability default", () => {
  const prior = process.env.PI_SEM_PROMPT;
  delete process.env.PI_SEM_PROMPT;
  try {
    assert.equal(resolvePromptShape(), "recipes");
  } finally {
    if (prior !== undefined) process.env.PI_SEM_PROMPT = prior;
  }
});

test("resolvePromptShape() reads \"table\" and \"dts\" from PI_SEM_PROMPT, falls back to \"recipes\" for anything unrecognized", () => {
  const prior = process.env.PI_SEM_PROMPT;
  try {
    process.env.PI_SEM_PROMPT = "table";
    assert.equal(resolvePromptShape(), "table");
    process.env.PI_SEM_PROMPT = "dts";
    assert.equal(resolvePromptShape(), "dts");
    process.env.PI_SEM_PROMPT = "garbage";
    assert.equal(resolvePromptShape(), "recipes");
  } finally {
    if (prior === undefined) delete process.env.PI_SEM_PROMPT;
    else process.env.PI_SEM_PROMPT = prior;
  }
});

test("shape A (table): the question->verb table alone, NO sem-api.d.ts contents", () => {
  const prompt = buildSystemPromptAddendum("table");
  assert.equal(prompt, CODE_MODE_ADDENDUM);
  assert.doesNotMatch(prompt, /declare const sem/, "table shape must not include sem-api.d.ts");
  assert.doesNotMatch(prompt, /Worked examples/, "table shape must not include the recipes");
});

test("shape B (dts): table + full sem-api.d.ts, no recipes -- the pre-flip default, kept as an opt-in", () => {
  const prompt = buildSystemPromptAddendum("dts");
  assert.match(prompt, /Code mode: call sem_code/, "must include the table");
  assert.match(prompt, /declare const sem/, "must include sem-api.d.ts");
  assert.doesNotMatch(prompt, /Worked examples/, "dts shape must not include the recipes");
});

test("shape C (recipes): table + d.ts + the five worked recipes", () => {
  const prompt = buildSystemPromptAddendum("recipes");
  assert.match(prompt, /Code mode: call sem_code/, "must include the table");
  assert.match(prompt, /declare const sem/, "must include sem-api.d.ts");
  assert.match(prompt, /Worked examples/, "must include the recipes");
  for (const verb of ["sem.rename", "sem.blast", "sem.why", "sem.edit", "sem.check", "sem.changed"]) {
    assert.match(prompt, new RegExp(verb.replace(".", "\\.")), `recipes must exercise ${verb}`);
  }
});

test("CODE_MODE_RECIPES: blast is the FIRST recipe, and its first line is literally the sem.blast(...) one-liner (verb-discovery priority)", () => {
  const body = CODE_MODE_RECIPES.replace(/^Worked examples.*\n\n/, "");
  const blocks = body.split(/\n\n+/).map((b) => b.trim());
  const firstBlock = blocks[0] ?? "";
  const firstLine = (firstBlock.split("\n")[0] ?? "").trim();
  assert.match(firstLine, /^const \w+ = await sem\.blast\(/, `expected the recipes' very first line to be the blast one-liner, got: ${firstLine}`);
});

test("CODE_MODE_RECIPES: exactly 5 recipes, each <=8 lines (comment + code, blank lines excluded)", () => {
  const body = CODE_MODE_RECIPES.replace(/^Worked examples.*\n\n/, "");
  const blocks = body
    .split(/\n\n+/)
    .map((b) => b.trim())
    .filter((b) => b.length > 0);
  assert.equal(blocks.length, 5, `expected exactly 5 worked recipes, got ${blocks.length}:\n${blocks.join("\n---\n")}`);
  for (const block of blocks) {
    const lines = block.split("\n").filter((l) => l.trim().length > 0);
    assert.ok(lines.length <= 8, `recipe exceeds the 8-line budget (${lines.length} lines):\n${block}`);
    assert.match(block.trim(), /^return |return /, `every recipe must end in a return: ${block}`);
  }
});

test("PI_SEM_PROMPT=table end-to-end: before_agent_start's system prompt excludes sem-api.d.ts entirely", async () => {
  const prior = process.env.PI_SEM_PROMPT;
  process.env.PI_SEM_PROMPT = "table";
  try {
    const { pi, handlers } = fakePi();
    registerSemCode(pi);
    const handler = handlers.get("before_agent_start");
    assert.ok(handler, "sem_code must register a before_agent_start handler");
    const result = (await handler({ systemPrompt: "BASE" }, {})) as { systemPrompt: string };
    assert.match(result.systemPrompt, /BASE/);
    assert.match(result.systemPrompt, /Code mode: call sem_code/);
    assert.doesNotMatch(result.systemPrompt, /declare const sem/);
  } finally {
    if (prior === undefined) delete process.env.PI_SEM_PROMPT;
    else process.env.PI_SEM_PROMPT = prior;
  }
});

test("PI_SEM_PROMPT=recipes end-to-end: before_agent_start's system prompt carries the table, the d.ts, AND the recipes", async () => {
  const prior = process.env.PI_SEM_PROMPT;
  process.env.PI_SEM_PROMPT = "recipes";
  try {
    const { pi, handlers } = fakePi();
    registerSemCode(pi);
    const handler = handlers.get("before_agent_start");
    assert.ok(handler, "sem_code must register a before_agent_start handler");
    const result = (await handler({ systemPrompt: "BASE" }, {})) as { systemPrompt: string };
    assert.match(result.systemPrompt, /BASE/);
    assert.match(result.systemPrompt, /Code mode: call sem_code/);
    assert.match(result.systemPrompt, /declare const sem/);
    assert.match(result.systemPrompt, /Worked examples/);
  } finally {
    if (prior === undefined) delete process.env.PI_SEM_PROMPT;
    else process.env.PI_SEM_PROMPT = prior;
  }
});
