import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { performSemRead } from "../../src/tools/sem-read.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

/**
 * OX-REVIEW-2 FINDING (confirmed independently before driving Ox's
 * supplementary native-tools pass, category b: "sem_read's budget path —
 * for hops=0, does the character-based slice risk cutting a UTF-16
 * surrogate pair in half, producing a malformed string?").
 *
 * src/tools/sem-read.ts's enforceBudget() (line ~170):
 *
 *   return { content: content.slice(0, Math.max(1, budget * 4)), tokens: budget, truncated: true };
 *
 * `.slice()` on a JS string cuts by UTF-16 CODE UNIT, not by Unicode code
 * point. Astral-plane characters (most emoji, some CJK Extension B+
 * characters, mathematical alphanumeric symbols) are encoded as a
 * surrogate PAIR (two 16-bit code units). When `budget * 4` lands on an
 * odd boundary in the middle of such a pair, the slice keeps only the
 * leading (high) surrogate — producing a string with a lone, unpaired
 * surrogate at the end. That's not valid UTF-16 text: JSON-encoding it
 * (as happens when the tool result is serialized to send to the model
 * API) either round-trips it as an invalid surrogate escape or gets
 * silently replaced with U+FFFD depending on the encoder, corrupting the
 * user's actual file content in the response the LLM sees — for a
 * genuinely truncated-but-otherwise-faithful excerpt to become silently
 * corrupted right at the cut point is a real content-integrity bug, not
 * just an aesthetic one.
 *
 * Confirmed empirically at the full tool level (not just the internal
 * helper) against this exact fixture + budget: budget=19 lands the
 * character cut mid-pair inside the emoji run, and the corruption survives
 * all the way into `performSemRead`'s final `outcome.text`.
 *
 * Untested: test/tools/read.test.ts's only budget test ("read truncates to
 * fit a small budget and reports it") uses a pure-ASCII fixture, so this
 * never surfaces there.
 *
 * Fix sketch: after slicing, trim a trailing unpaired high surrogate (or
 * do the truncation on an array of code points via `Array.from(content)`
 * instead of raw UTF-16 code units).
 */
test("sem_read's budget truncation does not split a UTF-16 surrogate pair, leaving a malformed string in the result", async () => {
  const outcome = await performSemRead(
    { file: "emoji.ts", entity: { name: "big" }, budget: 19 },
    { cwd: FIXTURES, semBin: "sem" },
  );

  assert.equal(outcome.isError, false, outcome.text);

  // A lone high surrogate (0xD800-0xDBFF) not immediately followed by its
  // low-surrogate partner (0xDC00-0xDFFF) means the string is malformed.
  const hasUnpairedSurrogate = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])/.test(outcome.text);
  assert.equal(
    hasUnpairedSurrogate,
    false,
    `sem_read's budget-truncated output contains an unpaired UTF-16 surrogate — the character-based slice in enforceBudget() cut a multi-byte emoji in half. Tail of result: ${JSON.stringify(outcome.text.slice(-20))}`,
  );
});
