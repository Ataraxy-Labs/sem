import { test } from "node:test";
import assert from "node:assert/strict";
import { deriveVisibility, compareIdentity, type IdentityFacts } from "../../src/tools/internal/identity.ts";

test("deriveVisibility reads TS/JS export prefixes", () => {
  assert.equal(deriveVisibility("math.ts", "add", "export function add() {"), "exported");
  assert.equal(deriveVisibility("math.ts", "add", "export default function add() {"), "exported");
  assert.equal(deriveVisibility("math.ts", "add", "function add() {"), "not-exported");
  assert.equal(deriveVisibility("math.js", "add", "  export function add() {"), "exported");
});

// A genuine regex literal containing an
// unescaped apostrophe used to be read as opening a plain string, which
// then ran to EOF (no other apostrophe to close it) and blanked a real,
// untouched `export { add };` further down -- a false negative with an
// unbounded blast radius. These exercise hasNamedExport's fileContent scan
// (via deriveVisibility's public entry point) directly against the
// regex-literal shapes stripCommentsAndStrings must now recognize.
test("deriveVisibility sees a real export-list statement even after a regex literal containing an apostrophe", () => {
  const fileContent = ["function add() {", "  const re = /don't match/;", "  return 1;", "}", "", "export { add };", ""].join("\n");
  assert.equal(deriveVisibility("math.ts", "add", "function add() {", fileContent), "exported");
});

test("deriveVisibility recognizes a regex literal right after `return`, not division", () => {
  const fileContent = ["function isBlank(s) {", "  return /^\\s*$/.test(s);", "}", "", "export { isBlank };", ""].join("\n");
  assert.equal(deriveVisibility("math.ts", "isBlank", "function isBlank(s) {", fileContent), "exported");
});

test("deriveVisibility does not mistake an ordinary division for a regex literal (no false positive)", () => {
  // `a / b` here is genuinely division; a regex-detector with too broad a
  // trigger could misparse this and corrupt the scan the same way the bug
  // did -- confirms division after an identifier stays division.
  const fileContent = ["function avg(a, b) {", "  return (a + b) / 2;", "}", "", "export { avg };", ""].join("\n");
  assert.equal(deriveVisibility("math.ts", "avg", "function avg(a, b) {", fileContent), "exported");
});

test("deriveVisibility handles a regex literal whose character class contains an unescaped `/`", () => {
  const fileContent = ["function isPath(s) {", "  return /^[a-z/]+$/.test(s);", "}", "", "export { isPath };", ""].join("\n");
  assert.equal(deriveVisibility("math.ts", "isPath", "function isPath(s) {", fileContent), "exported");
});

test("deriveVisibility: a mis-detected regex literal (no closing `/` on its own line) costs at most one line, not the whole file (defense in depth)", () => {
  // `isRegexLiteralPosition` treats a `/` right after `}` as regex-permitting
  // (a real tokenizer would need full bracket-matching to know whether that
  // `}` closed a block or an object-literal expression, which this small
  // heuristic deliberately doesn't attempt) -- so `{a:1} / x` here reads as
  // an (unterminated, no second `/` on the line) regex literal, not
  // division. The regex-skip loop bails at the newline rather than
  // searching past it, so a genuine export statement on a LATER line still
  // survives this known, accepted heuristic gap.
  const fileContent = ["function weird(x) {", "  return { a: 1 } / x;", "}", "", "export { weird };", ""].join("\n");
  assert.equal(deriveVisibility("math.ts", "weird", "function weird(x) {", fileContent), "exported");
});

test("deriveVisibility reads Rust pub prefixes, including pub(crate)", () => {
  assert.equal(deriveVisibility("lib.rs", "add", "pub fn add() {"), "exported");
  assert.equal(deriveVisibility("lib.rs", "add", "pub(crate) fn add() {"), "exported");
  assert.equal(deriveVisibility("lib.rs", "add", "fn add() {"), "not-exported");
});

test("deriveVisibility uses Go's capitalized-identifier convention", () => {
  assert.equal(deriveVisibility("math.go", "Add", "func Add() {"), "exported");
  assert.equal(deriveVisibility("math.go", "add", "func add() {"), "not-exported");
});

test("deriveVisibility uses Python's underscore-prefix convention", () => {
  assert.equal(deriveVisibility("math.py", "add", "def add():"), "exported");
  assert.equal(deriveVisibility("math.py", "_add", "def _add():"), "not-exported");
});

test("deriveVisibility is honestly unknown for unrecognized languages", () => {
  assert.equal(deriveVisibility("math.rb", "add", "def add"), "unknown");
});

function facts(overrides: Partial<IdentityFacts>): IdentityFacts {
  return { name: "add", type: "function", parentName: null, visibility: "exported", ...overrides };
}

test("compareIdentity finds nothing changed when name/type/parent/visibility all match", () => {
  assert.deepEqual(compareIdentity(facts({}), facts({})), []);
});

test("compareIdentity flags a visibility change", () => {
  const changes = compareIdentity(facts({ visibility: "exported" }), facts({ visibility: "not-exported" }));
  assert.equal(changes.length, 1);
  assert.equal(changes[0]?.field, "visibility");
});

test("compareIdentity never flags a change when either side's visibility is unknown", () => {
  assert.deepEqual(compareIdentity(facts({ visibility: "unknown" }), facts({ visibility: "not-exported" })), []);
  assert.deepEqual(compareIdentity(facts({ visibility: "exported" }), facts({ visibility: "unknown" })), []);
});

test("compareIdentity flags name, entity_type, and parent changes independently", () => {
  assert.equal(compareIdentity(facts({ name: "add" }), facts({ name: "plus" }))[0]?.field, "name");
  assert.equal(compareIdentity(facts({ type: "function" }), facts({ type: "variable" }))[0]?.field, "entity_type");
  assert.equal(compareIdentity(facts({ parentName: "Alice" }), facts({ parentName: "Bob" }))[0]?.field, "parent");
});

test("compareIdentity reports a missing after-entity as a change instead of silently passing", () => {
  const changes = compareIdentity(facts({}), undefined);
  assert.equal(changes.length, 1);
  assert.match(changes[0]?.after ?? "", /no entity found/);
});
