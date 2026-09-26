import assert from "node:assert/strict";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import {
  decideSessionRoute,
  describePlanCoverage,
  describeRepositoryCapabilities,
  normalizeRepositoryPath,
  partitionTransactionEdits,
} from "../../src/transaction/protocol-generalization.ts";

test("routes before session startup and fails open to native", () => {
  const structural = decideSessionRoute({
    task: "Rename this API and update all callers across the repository",
    files: ["src/api.ts", "src/client.ts"],
    indexWarm: true,
  });
  assert.equal(structural.protocol, "ataraxy.session-route.v1");
  assert.equal(structural.mode, "structural");
  assert.equal(structural.attach_structural_tools, true);
  assert.equal(structural.index, "warm");
  assert.equal(structural.fallback, "native");

  const localized = decideSessionRoute({ task: "Fix Language.__hash__ in langcodes/__init__.py" });
  assert.equal(localized.mode, "native");
  assert.deepEqual(localized.signals, ["explicit-source-path"]);

  const artifact = decideSessionRoute({ task: "Read JSONL records and create aggregates.json" });
  assert.equal(artifact.mode, "native");

  const unsupported = decideSessionRoute({
    task: "Update all callers across the repository",
    files: ["README.md", "assets/logo.svg"],
  });
  assert.equal(unsupported.mode, "native");
  assert.equal(unsupported.attach_structural_tools, false);
});

test("normalizes absolute and relative repository paths for sem search", () => {
  const root = path.join(os.tmpdir(), "repo");
  assert.equal(normalizeRepositoryPath(root, path.join(root, "src", "main.ts")), "src/main.ts");
  assert.equal(normalizeRepositoryPath(root, "src/main.ts"), "src/main.ts");
  assert.equal(normalizeRepositoryPath(root, "."), undefined);
  assert.equal(normalizeRepositoryPath(root, ""), undefined);
  assert.throws(() => normalizeRepositoryPath(root, path.dirname(root)), /escapes repository/);
});

test("profiles language confidence, validation, and benchmark task shape", () => {
  const source = describeRepositoryCapabilities(
    ["src/main.rs", "src/lib.rs", "README.md"],
    { kind: "cargo", typecheckCmd: ["cargo", "check"], testCmd: ["cargo", "test"] },
    "Fix the parser implementation and its failing test",
  );
  assert.equal(source.task_kind, "source");
  assert.equal(source.dominant_mutation_confidence, "high");
  assert.equal(source.validation.available, true);
  assert.equal(source.semantic_transaction_recommended, true);

  const terminal = describeRepositoryCapabilities(
    ["solver.py"], null, "Install a system package and aggregate the JSONL dataset into an output file",
  );
  assert.equal(terminal.task_kind, "environment");
  assert.equal(terminal.semantic_transaction_recommended, false);

  const cpp = describeRepositoryCapabilities(["src/main.cpp", "include/api.hpp"], null, "Change the API implementation");
  assert.equal(cpp.dominant_mutation_confidence, "guarded");

  const polyglot = describeRepositoryCapabilities(
    ["crates/Cargo.toml", "crates/src/lib.rs", "ui/package.json", "ui/src/app.ts"],
    { kind: "npm", testCmd: ["npm", "test"] },
    "Fix the implementation",
  );
  assert.equal(polyglot.validation.scope, "ambiguous");
  assert.deepEqual(polyglot.validation.ecosystems, ["cargo", "npm"]);
});

test("routes complete, partial, and empty plans explicitly", () => {
  const complete = describePlanCoverage({
    definitions: [{}], requestedNames: ["run"], resolvedNames: new Set(["run"]), matches: null,
  });
  assert.equal(complete.recommended_mode, "transaction");
  assert.equal(complete.recovery_allowed, false);

  const partial = describePlanCoverage({
    definitions: [{}], requestedNames: ["run", "stop"], resolvedNames: new Set(["run"]), matches: null,
  });
  assert.equal(partial.recommended_mode, "hybrid");

  const empty = describePlanCoverage({
    definitions: [], requestedNames: ["missing"], resolvedNames: new Set(), matches: { results: [] },
  });
  assert.equal(empty.recommended_mode, "native_fallback");
  assert.equal(empty.recovery_allowed, true);
  assert.equal(empty.capabilities.entity_replacement, "unavailable");
});

test("partitions large transactions without splitting a file's edits", () => {
  const edits = [
    { file: "a.rs", id: 1 },
    { file: "b.rs", id: 2 },
    { file: "a.rs", id: 3 },
    { file: "c.rs", id: 4 },
    { file: "d.rs", id: 5 },
  ];
  const batches = partitionTransactionEdits(edits, 2, 3);
  assert.deepEqual(batches, [
    [{ file: "a.rs", id: 1 }, { file: "a.rs", id: 3 }, { file: "b.rs", id: 2 }],
    [{ file: "c.rs", id: 4 }, { file: "d.rs", id: 5 }],
  ]);
});

test("transaction partitioning validates limits and keeps oversized file groups intact", () => {
  const edits = Array.from({ length: 4 }, (_, id) => ({ file: "one.rs", id }));
  assert.deepEqual(partitionTransactionEdits(edits, 2, 2), [edits]);
  assert.throws(() => partitionTransactionEdits(edits, 0, 2), /positive integer/);
  assert.throws(() => partitionTransactionEdits(edits, 2, 0), /positive integer/);
});
