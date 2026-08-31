import { test } from "node:test";
import assert from "node:assert/strict";
import { splice } from "../../src/tools/internal/text.ts";

/**
 * OX-REVIEW-2 FINDING (confirmed independently before driving Ox, category
 * c: "the new splice() padding logic in text.ts — check whether the
 * reported newRange and insertedText actually match what was written to
 * disk when padding was added").
 *
 * src/tools/internal/text.ts's splice(), op === "replace" branch
 * (introduced by commit 9f6f721, "splice replace preserves a trailing
 * blank separator line"):
 *
 *   const targetTrailingBlanks = countTrailingBlank(target);
 *   const paddedReindented =
 *     targetTrailingBlanks > 0 && countTrailingBlank(reindented) === 0
 *       ? [...reindented, ...Array<string>(targetTrailingBlanks).fill("")]
 *       : reindented;
 *   const merged = [...before, ...paddedReindented, ...after];
 *   return {
 *     text: renderLines(model, merged),                                   // uses paddedReindented (correct)
 *     newRange: { start: before.length + 1, end: before.length + reindented.length },  // uses UNPADDED reindented.length
 *     insertedText,                                                       // = reindented.join(...) — also UNPADDED
 *   };
 *
 * `text` (what actually lands on disk) is built from `paddedReindented`,
 * but `newRange.end` and `insertedText` are both computed from the
 * unpadded `reindented` — under-reporting by exactly `targetTrailingBlanks`
 * whenever padding is applied. The padding blank line is not scratch
 * whitespace: per this same commit's own doc comment, sem reports a
 * markdown heading's (or similar entity's) `end_line` as extending through
 * that trailing blank — i.e. the blank line is part of the entity's
 * declared range, both before AND after the edit, by sem's own semantics.
 * Under-reporting it is therefore a real inconsistency, not a rounding
 * nicety:
 *
 * 1. weave-edit.ts's formatSuccess() reports the user-facing "lines X-Y ->
 *    A-B" range using this newRange — the reported end line is one (or
 *    more) lines short of where the replacement content actually ends on
 *    disk.
 * 2. weave-edit.ts's performWeaveEdit() sends this `insertedText` to
 *    weave-mcp's `weave_update_entity_content` as the entity's new content
 *    (queueOutcome.insertedText, used verbatim as `newContent` in the
 *    coordinator.updateAndRelease() call) — so weave-mcp's own record of
 *    "what this entity's content now is" would omit the trailing blank
 *    line that IS actually on disk, a genuine content mismatch between
 *    weave-mcp's bookkeeping and the file.
 *
 * Untested: the existing test ("splice replace: preserves the trailing
 * blank line before the next markdown heading…", text.test.ts) only
 * asserts `result.text`; it never inspects `result.newRange` or
 * `result.insertedText`, so this discrepancy has no coverage.
 */
test("splice replace: newRange.end and insertedText include the padded trailing blank line, matching what's actually written to disk", () => {
  const original = ["# First Section", "", "Some content here.", "", "## Second Section", "", "More content."].join("\n");
  const result = splice(original, { start_line: 1, end_line: 4 }, "replace", "# First Section\n\nUpdated content.");

  // Ground truth: re-derive what actually landed on disk at the reported range.
  const actualLines = result.text.split("\n");
  assert.ok(result.newRange, "replace must report a newRange");

  const linesAtReportedRange = actualLines.slice(result.newRange!.start - 1, result.newRange!.end);
  const linesActuallyWrittenByThisOp = actualLines.slice(0, actualLines.indexOf("## Second Section"));

  assert.deepEqual(
    linesAtReportedRange,
    linesActuallyWrittenByThisOp,
    `newRange (${JSON.stringify(result.newRange)}) does not cover everything this replace actually wrote before "## Second Section" ` +
      `— it's missing the padded trailing blank line. On-disk lines up to the next heading: ${JSON.stringify(linesActuallyWrittenByThisOp)}, ` +
      `but newRange only covers: ${JSON.stringify(linesAtReportedRange)}`,
  );

  assert.equal(
    result.insertedText,
    linesActuallyWrittenByThisOp.join("\n"),
    `insertedText (sent to weave-mcp as the entity's new content) must match what was actually written to disk for this entity, ` +
      `including the padded trailing blank line`,
  );
});
