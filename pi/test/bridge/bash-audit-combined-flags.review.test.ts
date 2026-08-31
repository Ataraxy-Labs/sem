import { test } from "node:test";
import assert from "node:assert/strict";
import { classifyBashCommand } from "../../src/bridge/bash-audit.ts";

/**
 * OX-REVIEW-3 FINDING (found independently, category "write audit + bash
 * classifier: which NEW evasions remain even after this round's
 * broadening").
 *
 * bash-audit.ts's "indirect" rule, added this round (commit 53d75b0,
 * "broaden the bash audit classifier: indirect shells..."):
 *
 *   pattern: /^(sh|bash)\s+-c\b|^python3?\s+-c\b|^node\s+-e\b/
 *
 * This only matches a BARE, space-separated `-c`/`-e` token. Combined
 * single-character shell flags — extremely ordinary, common shell usage,
 * not an exotic adversarial trick — evade it entirely: `bash -lc "..."`
 * (login shell + command, arguably the single most common way people
 * actually invoke `bash -c` from scripts and dotfiles), `bash -ic "..."`
 * (interactive + command), `sh -lc "..."`. None of these are in the
 * module's own disclosed-gaps list (env/alias/function wrappers, nested
 * substitution, xargs/find-exec, heredoc bodies, external script files,
 * unusual `sed -i` spelling) — this is a genuinely new, undisclosed gap in
 * the very rule this round's diff added specifically to close shell
 * indirection.
 *
 * Confirmed empirically: `classifyBashCommand('bash -lc "cat secret.env"')`
 * returns `[]` (no match at all) — the exact class of command the
 * "indirect" rule exists to catch, evading it via a one-character flag
 * combination.
 *
 * Untested: bash-audit.test.ts's indirect-shell tests only exercise bare
 * `-c`/`-e` (matching the rule's own literal pattern), never a combined
 * flag form.
 *
 * Fix sketch: match any token starting with `-` that CONTAINS a `c`
 * (for sh/bash) — e.g. `/^(sh|bash)\s+-\w*c\w*\b/` — rather than requiring
 * `-c` as a standalone token; same idea for python's `-c`/node's `-e`
 * combined with other single-char flags like `-i` (interactive).
 */
test("bash -lc / -ic (combined short flags) evade the 'indirect shell' rule entirely, unlike bare -c", async () => {
  const evasions = ['bash -lc "cat secret.env"', 'sh -lc "cat secret.env"', 'bash -ic "cat secret.env"'];
  for (const cmd of evasions) {
    const matches = classifyBashCommand(cmd);
    assert.ok(
      matches.length > 0,
      `expected "${cmd}" to be classified as indirect-shell (the same class bare "bash -c" already is), but classifyBashCommand returned zero matches — combined short flags evade the rule entirely.`,
    );
  }
});
