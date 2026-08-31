import { test } from "node:test";
import assert from "node:assert/strict";
import { computeToolPayloadSize } from "../../src/bridge/tool-payload-size.ts";

// Spec for the tool-definition payload size the model actually sees per
// turn: for every ACTIVE tool (built-ins included), the JSON size of
// {name, description, parameters} -- chars, and tokens estimated as
// round(chars / 4). Deliberately tests invariants rather than hand-counted
// exact character totals, so the spec doesn't silently re-derive (and
// therefore tautologically match) the implementation's own JSON shape.

const ALL_TOOLS = [
  { name: "bash", description: "Execute bash commands", parameters: { type: "object", properties: {} } },
  { name: "write", description: "Write a file to disk, overwriting it if present", parameters: { type: "object", properties: { path: { type: "string" } } } },
  {
    name: "sem_impact",
    description:
      "Unified blast-radius analysis for one entity: its direct dependencies, direct dependents, transitive impact, and the tests that exercise it.",
    parameters: { type: "object", properties: { entity_name: { type: "string" }, file_path: { type: "string" } } },
  },
];

test("toolCount matches the number of active names that have a matching definition", () => {
  const summary = computeToolPayloadSize(["bash", "sem_impact"], ALL_TOOLS);
  assert.equal(summary.toolCount, 2);
  assert.deepEqual(
    summary.perTool.map((t) => t.name),
    ["bash", "sem_impact"],
  );
});

test("an inactive tool contributes nothing, regardless of its position in allTools", () => {
  const withWrite = computeToolPayloadSize(["bash", "write"], ALL_TOOLS);
  const withoutWrite = computeToolPayloadSize(["bash"], ALL_TOOLS);
  assert.equal(withoutWrite.toolCount, 1);
  assert.ok(withWrite.totalChars > withoutWrite.totalChars, "adding an active tool should only ever increase the total");
});

test("an active name with no matching tool definition is skipped, not a crash", () => {
  const summary = computeToolPayloadSize(["bash", "does_not_exist"], ALL_TOOLS);
  assert.equal(summary.toolCount, 1);
  assert.deepEqual(
    summary.perTool.map((t) => t.name),
    ["bash"],
  );
});

test("each tool's tokens is round(chars / 4)", () => {
  const summary = computeToolPayloadSize(["bash", "sem_impact"], ALL_TOOLS);
  for (const tool of summary.perTool) {
    assert.equal(tool.tokens, Math.round(tool.chars / 4));
  }
});

test("totals are the sum of the per-tool values", () => {
  const summary = computeToolPayloadSize(["bash", "write", "sem_impact"], ALL_TOOLS);
  assert.equal(
    summary.totalChars,
    summary.perTool.reduce((sum, t) => sum + t.chars, 0),
  );
  assert.equal(
    summary.totalTokens,
    summary.perTool.reduce((sum, t) => sum + t.tokens, 0),
  );
});

test("a tool with a longer description/parameters has more chars than one with less", () => {
  const summary = computeToolPayloadSize(["bash", "sem_impact"], ALL_TOOLS);
  const bash = summary.perTool.find((t) => t.name === "bash");
  const semImpact = summary.perTool.find((t) => t.name === "sem_impact");
  assert.ok(bash && semImpact);
  assert.ok(semImpact.chars > bash.chars, "sem_impact's much longer description/parameters should cost more chars than bash's");
});

test("zero active tools returns a zeroed, non-crashing summary", () => {
  const summary = computeToolPayloadSize([], ALL_TOOLS);
  assert.deepEqual(summary, { toolCount: 0, totalChars: 0, totalTokens: 0, perTool: [] });
});

test("a tool definition missing description/parameters is measured as if they were empty, not a crash", () => {
  const summary = computeToolPayloadSize(["bare"], [{ name: "bare" }]);
  assert.equal(summary.toolCount, 1);
  assert.ok(summary.totalChars > 0);
});
