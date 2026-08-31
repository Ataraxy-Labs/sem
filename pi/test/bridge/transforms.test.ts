import { test } from "node:test";
import assert from "node:assert/strict";
import { applyTransform } from "../../src/bridge/transforms.ts";

// "outline"'s documented target shape: a flat JSON array of
// {type, name, parent_id?, start_line, end_line, start_byte?, end_byte?},
// nested by matching parent_id to another entity's name. No live MCP tool
// in this bridge emits exactly this shape today (see the commit message
// this file ships with) -- these fixtures are the documented shape itself,
// plus two tests using output captured verbatim from the real servers to
// pin down today's actual (safely degraded) behavior.

test("outline: renders a nested TS-shaped outline with token estimates and a header", () => {
  const entities = [
    { type: "class", name: "Calculator", parent_id: null, start_line: 1, end_line: 12, start_byte: 0, end_byte: 200 },
    { type: "field", name: "total", parent_id: "Calculator", start_line: 2, end_line: 2, start_byte: 20, end_byte: 28 },
    { type: "method", name: "addToTotal", parent_id: "Calculator", start_line: 4, end_line: 7, start_byte: 40, end_byte: 120 },
    { type: "function", name: "add", parent_id: null, start_line: 14, end_line: 16, start_byte: 300, end_byte: 340 },
  ];
  const out = applyTransform("outline", JSON.stringify(entities), { toolName: "x", args: { file_path: "sample.ts" } });

  assert.equal(
    out,
    [
      "sample.ts: 4 entities, ~82 tokens",
      "class Calculator [L1-L12] ~50",
      "  field total [L2-L2] ~2",
      "  method addToTotal [L4-L7] ~20",
      "function add [L14-L16] ~10",
    ].join("\n"),
  );
});

test("outline: renders a nested markdown-heading-shaped outline", () => {
  const entities = [
    { type: "h1", name: "Introduction", parent_id: null, start_line: 1, end_line: 10, start_byte: 0, end_byte: 40 },
    { type: "h2", name: "Background", parent_id: "Introduction", start_line: 3, end_line: 5, start_byte: 8, end_byte: 16 },
    { type: "h2", name: "Scope", parent_id: "Introduction", start_line: 6, end_line: 9, start_byte: 16, end_byte: 24 },
    { type: "h1", name: "Usage", parent_id: null, start_line: 11, end_line: 20, start_byte: 40, end_byte: 80 },
  ];
  const out = applyTransform("outline", JSON.stringify(entities), { toolName: "x", args: { file_path: "README.md" } });

  assert.equal(
    out,
    [
      "README.md: 4 entities, ~24 tokens",
      "h1 Introduction [L1-L10] ~10",
      "  h2 Background [L3-L5] ~2",
      "  h2 Scope [L6-L9] ~2",
      "h1 Usage [L11-L20] ~10",
    ].join("\n"),
  );
});

test("outline: an entity with no start_byte/end_byte omits the token estimate instead of guessing", () => {
  const entities = [{ type: "function", name: "f", start_line: 1, end_line: 2 }];
  const out = applyTransform("outline", JSON.stringify(entities), { toolName: "x", args: {} });
  assert.equal(out, ["(unknown file): 1 entities, ~0 tokens", "function f [L1-L2]"].join("\n"));
});

test("outline: a dangling parent_id (no entity has that name) renders at the top level, not dropped", () => {
  const entities = [{ type: "method", name: "orphan", parent_id: "NoSuchParent", start_line: 1, end_line: 2 }];
  const out = applyTransform("outline", JSON.stringify(entities), { toolName: "x", args: {} });
  assert.equal(out, ["(unknown file): 1 entities, ~0 tokens", "method orphan [L1-L2]"].join("\n"));
});

test("outline: caps the rendered list at 200 lines with a count-and-hint tail, but the header keeps the full count", () => {
  const entities = Array.from({ length: 205 }, (_, i) => ({
    type: "function",
    name: `f${i}`,
    start_line: i + 1,
    end_line: i + 1,
  }));
  const out = applyTransform("outline", JSON.stringify(entities), { toolName: "x", args: { path: "big.ts" } });
  const lines = out.split("\n");

  assert.equal(lines[0], "big.ts: 205 entities, ~0 tokens");
  assert.equal(lines.length, 1 + 200 + 1); // header + 200 entity lines + tail
  assert.equal(lines.at(-1), "…5 more; filter with text=");
});

test("outline: an unparseable-as-outline result (real sem_entities text, sem 0.23.0, no JSON mode) passes through unchanged", () => {
  const realSemEntitiesOutput = "⊕ 3 entities · calculator.ts\nCalculator · class · L3-10\n  total · field · L4\n  addToTotal · method · L6-9\n";
  const out = applyTransform("outline", realSemEntitiesOutput, { toolName: "sem_entities", args: { path: "calculator.ts" } });
  assert.equal(out, realSemEntitiesOutput);
});

test("outline: real weave_extract_entities JSON (no parent_id/start_byte/end_byte in its actual schema) still renders a flat, token-less outline", () => {
  // Captured verbatim from a live weave-mcp tools/call against calculator.ts
  // (see src/tools/weave-edit.ts's fixture set). parent_id/start_byte/end_byte
  // are optional in the documented outline shape, so this DOES parse -- just
  // without nesting (weave_extract_entities' nesting lives in its `::`
  // delimited `id` field, not a parent_id this transform reads) or token
  // estimates (no byte offsets in this tool's real output).
  const realWeaveExtractEntitiesOutput = JSON.stringify([
    { end_line: 10, id: "calculator.ts::class::Calculator", name: "Calculator", start_line: 3, type: "class" },
    { end_line: 4, id: "calculator.ts::class::Calculator::total", name: "total", start_line: 4, type: "field" },
    { end_line: 9, id: "calculator.ts::class::Calculator::addToTotal", name: "addToTotal", start_line: 6, type: "method" },
  ]);
  const out = applyTransform("outline", realWeaveExtractEntitiesOutput, {
    toolName: "weave_extract_entities",
    args: { file_path: "calculator.ts" },
  });

  assert.equal(
    out,
    [
      "calculator.ts: 3 entities, ~0 tokens",
      "class Calculator [L3-L10]",
      "field total [L4-L4]",
      "method addToTotal [L6-L9]",
    ].join("\n"),
  );
});

test("applyTransform: an unknown transform id passes text through unchanged instead of erroring", () => {
  const out = applyTransform("not-a-real-transform", "hello", { toolName: "x", args: {} });
  assert.equal(out, "hello");
});

test("applyTransform: no transform id passes text through unchanged", () => {
  const out = applyTransform(undefined, "hello", { toolName: "x", args: {} });
  assert.equal(out, "hello");
});

test("outline: non-array JSON (e.g. an error object) passes through unchanged", () => {
  const out = applyTransform("outline", JSON.stringify({ error: "not found" }), { toolName: "x", args: {} });
  assert.equal(out, JSON.stringify({ error: "not found" }));
});
