import { test } from "node:test";
import assert from "node:assert/strict";
import { parseLines, renderLines, reindent, splice } from "../../src/tools/internal/text.ts";

test("parseLines detects LF, CRLF, and trailing-newline presence", () => {
  const lf = parseLines("a\nb\nc\n");
  assert.deepEqual(lf, { eol: "\n", hasTrailingNewline: true, lines: ["a", "b", "c"] });

  const lfNoTrailing = parseLines("a\nb\nc");
  assert.deepEqual(lfNoTrailing, { eol: "\n", hasTrailingNewline: false, lines: ["a", "b", "c"] });

  const crlf = parseLines("a\r\nb\r\nc\r\n");
  assert.deepEqual(crlf, { eol: "\r\n", hasTrailingNewline: true, lines: ["a", "b", "c"] });

  const crlfNoTrailing = parseLines("a\r\nb\r\nc");
  assert.deepEqual(crlfNoTrailing, { eol: "\r\n", hasTrailingNewline: false, lines: ["a", "b", "c"] });
});

test("renderLines round-trips exactly through parseLines", () => {
  for (const original of ["a\nb\nc\n", "a\nb\nc", "a\r\nb\r\nc\r\n", "a\r\nb\r\nc", ""]) {
    const model = parseLines(original);
    assert.equal(renderLines(model, model.lines), original);
  }
});

test("reindent strips the content's own common indentation and applies the target indent", () => {
  const content = ["  function greet() {", "    return 1;", "  }"];
  assert.deepEqual(reindent(content, "    "), ["    function greet() {", "      return 1;", "    }"]);
  assert.deepEqual(reindent(content, ""), ["function greet() {", "  return 1;", "}"]);
});

test("reindent leaves blank lines blank instead of padding them", () => {
  const content = ["function greet() {", "", "  return 1;", "}"];
  assert.deepEqual(reindent(content, "  "), ["  function greet() {", "", "    return 1;", "  }"]);
});

test("splice replace: swaps the entity's lines and re-indents to the anchor's indentation", () => {
  const original = ["class Bob {", "  greet() {", "    return 1;", "  }", "}"].join("\n");
  const result = splice(original, { start_line: 2, end_line: 4 }, "replace", "greet() {\n  return 2;\n}");
  assert.equal(result.text, ["class Bob {", "  greet() {", "    return 2;", "  }", "}"].join("\n"));
  assert.deepEqual(result.newRange, { start: 2, end: 4 });
  assert.equal(result.insertedText, "  greet() {\n    return 2;\n  }");
});

test("splice insert_after: adds a blank-line-separated block at the anchor's indentation", () => {
  const original = ["function foo() {", "  return 1;", "}", "function bar() {", "  return 2;", "}"].join("\n");
  const result = splice(original, { start_line: 1, end_line: 3 }, "insert_after", "function mid() {\n  return 99;\n}");
  assert.equal(
    result.text,
    ["function foo() {", "  return 1;", "}", "", "function mid() {", "  return 99;", "}", "", "function bar() {", "  return 2;", "}"].join("\n"),
  );
  assert.deepEqual(result.newRange, { start: 5, end: 7 });
});

test("splice insert_before: adds a blank-line-separated block ahead of the anchor", () => {
  const original = ["function foo() {", "  return 1;", "}"].join("\n");
  const result = splice(original, { start_line: 1, end_line: 3 }, "insert_before", "function pre() {\n  return 0;\n}");
  assert.equal(result.text, ["function pre() {", "  return 0;", "}", "", "function foo() {", "  return 1;", "}"].join("\n"));
  assert.deepEqual(result.newRange, { start: 1, end: 3 });
});

test("splice insert_after skips the blank separator when one already exists", () => {
  const original = ["function foo() {", "  return 1;", "}", ""].join("\n");
  const result = splice(original, { start_line: 1, end_line: 3 }, "insert_after", "function bar() {\n  return 2;\n}");
  assert.equal(result.text, ["function foo() {", "  return 1;", "}", "", "function bar() {", "  return 2;", "}", ""].join("\n"));
});

test("splice delete: removes the entity's lines and collapses a resulting double blank line", () => {
  const original = ["function foo() {", "  return 1;", "}", "", "function bar() {", "  return 2;", "}", "", "function baz() {", "  return 3;", "}"].join(
    "\n",
  );
  const result = splice(original, { start_line: 5, end_line: 7 }, "delete", undefined);
  assert.equal(result.text, ["function foo() {", "  return 1;", "}", "", "function baz() {", "  return 3;", "}"].join("\n"));
  assert.equal(result.newRange, undefined);
  assert.equal(result.insertedText, undefined);
});

test("splice replace: preserves the trailing blank line before the next markdown heading when the new content doesn't end with one", () => {
  // sem reports a markdown heading's own end_line as extending through the
  // blank separator line before the next heading (confirmed empirically: a
  // "# First Section" heading followed by a blank line then "## Second
  // Section" gets end_line = the blank line, not the last non-blank line).
  // That blank line is therefore part of `target`, not `after` — a naive
  // replace (just [...before, ...reindented, ...after]) drops it whenever
  // the replacement content doesn't itself end with a blank line.
  const original = ["# First Section", "", "Some content here.", "", "## Second Section", "", "More content."].join("\n");
  const result = splice(original, { start_line: 1, end_line: 4 }, "replace", "# First Section\n\nUpdated content.");
  assert.equal(
    result.text,
    ["# First Section", "", "Updated content.", "", "## Second Section", "", "More content."].join("\n"),
    "the blank line separating the replaced section from the next heading must survive",
  );
});

test("splice replace: does not add an extra blank line when the new content already ends with one", () => {
  const original = ["# First Section", "", "Some content here.", "", "## Second Section", "", "More content."].join("\n");
  const result = splice(original, { start_line: 1, end_line: 4 }, "replace", "# First Section\n\nUpdated content.\n");
  assert.equal(
    result.text,
    ["# First Section", "", "Updated content.", "", "## Second Section", "", "More content."].join("\n"),
    "already-present trailing blank line should not be doubled",
  );
});

test("splice preserves CRLF and a missing trailing newline through a replace", () => {
  const original = "function foo() {\r\n  return 1;\r\n}\r\nfunction bar() {\r\n  return 2;\r\n}";
  const result = splice(original, { start_line: 1, end_line: 3 }, "replace", "function foo() {\n  return 11;\n}");
  assert.equal(result.text, "function foo() {\r\n  return 11;\r\n}\r\nfunction bar() {\r\n  return 2;\r\n}");
  assert.ok(!result.text.endsWith("\n"));
});
