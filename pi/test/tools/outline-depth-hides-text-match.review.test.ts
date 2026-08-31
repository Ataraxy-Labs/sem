import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { performSemOutline } from "../../src/tools/sem-outline.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * OX-REVIEW-2 FINDING (confirmed independently before driving Ox's
 * supplementary native-tools pass, category b: "does requesting depth=N
 * together with text= interact correctly, or does the depth ceiling get
 * applied ... in a way that could hide a real match?").
 *
 * src/tools/sem-outline.ts's performSemOutline():
 *
 *   keptPredicate(e) is true if e's OWN name matches text=, OR ANY
 *   DESCENDANT at any depth matches — this is what lets an ancestor chain
 *   stay visible as "context" for a deep match (the documented purpose of
 *   text=, per the schema: "enough ancestor context ... to locate them").
 *
 *   collect(e, depth) — the function that actually builds the rendered
 *   list — returns early once `depth > params.depth` (line ~196), BEFORE
 *   ever checking whether `e` itself is the thing that matched. So when
 *   the only entity matching text= lives deeper than depth= allows: its
 *   ancestors get rendered (correctly identified as "kept" by
 *   keptPredicate, since a descendant matches) — but the matching entity
 *   itself never reaches `allEntries` at all, so it's not just omitted
 *   from the top MAX_RENDERED_LINES cut, it never existed as a candidate
 *   to render or count in the first place. The result: a tree branch that
 *   dead-ends exactly where the match would have appeared, with nothing to
 *   tell the caller a match was hidden — `shownCount` (used in the header)
 *   counts the entity as "shown" even though it never renders, and
 *   `omitted`/`details.truncated` are computed from `allEntries`, which
 *   never included the hidden entity either, so the "…N more" truncation
 *   hint never fires. text= exists specifically so a caller can locate an
 *   entity by name without reading the whole file; depth= silently
 *   defeating that for the exact entity being searched for is the tool
 *   failing at its one job, with no error and no signal anything is wrong.
 *
 * Untested: outline.test.ts's depth test ("depth caps how deep the tree
 * renders...") and text= test ("text= filters to matching entities...")
 * each exercise depth= and text= independently; nothing combines them.
 */
test("sem_outline with text= and a shallow depth= silently omits the only entity that actually matched, with no truncation signal", async () => {
  const dir = mkdtempSync(join(tmpdir(), "outline-depth-text-"));
  try {
    writeFileSync(
      join(dir, "deep.ts"),
      [
        "export namespace NS {",
        "  export class Inner {",
        "    deepTargetMatch(): number {",
        "      return 2;",
        "    }",
        "  }",
        "}",
        "",
      ].join("\n"),
      "utf8",
    );

    const outcome = await performSemOutline({ file: "deep.ts", text: "deepTargetMatch", depth: 2 }, { cwd: dir, semBin: "sem" });

    assert.equal(outcome.isError, false, outcome.text);

    // The header line always echoes the text= value back ('matching
    // "deepTargetMatch": N shown'), so a bare substring check against the
    // whole text is not enough — it would pass even when the entity itself
    // never renders. Check the tree BODY (every line after the header) for
    // either the matching entity's own line, or at least a truncation
    // signal ("…N more") that would tell the caller something was hidden.
    const bodyLines = outcome.text.split("\n").slice(1);
    const body = bodyLines.join("\n");
    const entityLineShown = /\bdeepTargetMatch\b/.test(body);
    const truncationSignaled = /…\d+ more/.test(body) || (outcome.details as { truncated?: boolean }).truncated === true;

    assert.ok(
      entityLineShown || truncationSignaled,
      `text="deepTargetMatch" matched exactly one entity, but it does not appear anywhere in the rendered tree AND there is no ` +
        `truncation signal telling the caller something was hidden by depth= — the match is silently invisible. Full output:\n${outcome.text}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
