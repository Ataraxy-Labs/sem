import { test } from "node:test";
import assert from "node:assert/strict";
import { auditWriteCommand, isCodeFilePath } from "../../src/bridge/write-audit.ts";

test("isCodeFilePath recognizes common source extensions", () => {
  assert.equal(isCodeFilePath("src/foo.ts"), true);
  assert.equal(isCodeFilePath("src/foo.py"), true);
  assert.equal(isCodeFilePath("src/foo.rs"), true);
  assert.equal(isCodeFilePath("src/foo.go"), true);
});

test("isCodeFilePath is case-insensitive on the extension", () => {
  assert.equal(isCodeFilePath("src/Foo.TS"), true);
});

test("isCodeFilePath rejects non-code extensions", () => {
  assert.equal(isCodeFilePath("README.md"), false);
  assert.equal(isCodeFilePath("data.json"), false);
  assert.equal(isCodeFilePath("notes.txt"), false);
  assert.equal(isCodeFilePath("image.png"), false);
});

test("isCodeFilePath rejects extensionless files", () => {
  assert.equal(isCodeFilePath("Makefile"), false);
  assert.equal(isCodeFilePath("Dockerfile"), false);
});

test("auditWriteCommand never refuses when PI_SEM_STRICT is off, even for an existing code file", () => {
  const decision = auditWriteCommand("src/existing.ts", 100, true, false);
  assert.equal(decision.refuse, false);
  assert.equal(decision.entry.isCodeFile, true);
  assert.equal(decision.entry.bytes, 100);
});

test("auditWriteCommand never refuses a write to a brand-new file, strict or not", () => {
  const strictDecision = auditWriteCommand("src/new.ts", 50, false, true);
  assert.equal(strictDecision.refuse, false);

  const auditDecision = auditWriteCommand("src/new.ts", 50, false, false);
  assert.equal(auditDecision.refuse, false);
});

test("auditWriteCommand never refuses a write to an existing non-code file, even under strict mode", () => {
  const decision = auditWriteCommand("README.md", 50, true, true);
  assert.equal(decision.refuse, false);
  assert.equal(decision.entry.isCodeFile, false);
});

test("auditWriteCommand refuses a write to an existing code file under strict mode, with the exact required message substring", () => {
  const decision = auditWriteCommand("src/existing.ts", 200, true, true);
  assert.equal(decision.refuse, true);
  assert.ok(decision.refusalMessage);
  assert.ok(
    decision.refusalMessage.includes("use weave_edit for existing entities; write is for new files"),
    `refusal message must contain the required guidance, got: ${decision.refusalMessage}`,
  );
});

test("auditWriteCommand records path/bytes/isCodeFile in its entry regardless of refusal", () => {
  const decision = auditWriteCommand("src/existing.rs", 42, true, true);
  assert.deepEqual(decision.entry, { path: "src/existing.rs", bytes: 42, isCodeFile: true });
});
