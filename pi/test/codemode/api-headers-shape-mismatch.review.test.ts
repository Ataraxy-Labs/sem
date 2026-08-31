import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { buildSemApi } from "../../src/codemode/api.ts";

function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-api-headers-test-"));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * This round's brief flagged this exact question:
 * "we already independently found that headers() was missing name/type/
 * signature in an earlier check -- verify whether that's actually still
 * true". Confirmed: still true, and worse than "missing a few fields" —
 * the actual shape doesn't resemble the declared one at all.
 *
 * src/codemode/sem-api.d.ts declares `sem.headers()` returns `HeaderLine[]`:
 *
 *   declare interface HeaderLine {
 *     name: string; type: string; file: string; parent_name: string | null;
 *     signature: string;   // the entity's own signature/declaration line, trimmed
 *     doc: string | null;  // the doc-comment line immediately preceding it
 *   }
 *
 * This .d.ts is injected VERBATIM into the system prompt every code-mode
 * session sees (tool.ts's before_agent_start) — it IS the contract the
 * model is told to rely on, with no other documentation of the shape
 * anywhere. But `headers()` (api.ts) never constructs a HeaderLine at all —
 * it composes `performSemOutline` + `performSemRead(..., mode: "headers")`
 * and returns each call's raw `{...outcome.details, content: outcome.text}`
 * unmodified (see headersForFile/headers in api.ts). The ACTUAL runtime
 * shape, confirmed empirically against a real fixture:
 *
 *   { file, entity: {name,type,parent_name,start_line,end_line},
 *     range_source, related, truncated, mode, content }
 *
 * Every field the .d.ts promises is either missing or in the wrong place:
 *  - `name`/`type`/`parent_name` are NOT top-level; they're nested under
 *    `entity` instead.
 *  - `signature` does not exist ANYWHERE. The closest analog is buried
 *    inside `content`, which is not the clean signature line the .d.ts
 *    promises — it's sem_read's full user-facing prose, reused unmodified
 *    for headers mode too (`buildSuccessText` in sem-read.ts): a leading
 *    "sem_read: add (function) in math.ts, lines 2-4" line, then a blank
 *    line, then the doc/signature text, then a trailing "[source: headers]"
 *    provenance line — all one string, not separated into signature vs doc.
 *  - `doc` does not exist anywhere either, for the same reason.
 *
 * A script written straight off the injected .d.ts — e.g.
 * `headers.map(h => h.signature)` or `for (const h of await sem.headers(f))
 * console.log(h.name, h.doc)` — silently gets `undefined` for every one of
 * those accesses. This isn't a partial-completeness gap; the declared and
 * actual shapes are two different data structures that happen to both be
 * JSON objects with a `file` key.
 *
 * Untested: there is no api-headers.test.ts (api-read/api-write/api-edit
 * each get their own file; headers() has none at all).
 *
 * Fix sketch: either make headers() actually build a HeaderLine (extract
 * the entity's signature/doc summary as clean fields, matching what
 * sem_read's own internal extractHeader()/leadingDocSummary() already
 * compute BEFORE they get folded into prose by buildSuccessText — those
 * are the right primitives, just never surfaced structurally), or fix
 * sem-api.d.ts to declare the shape headers() actually returns.
 */
test("sem.headers() does not return the HeaderLine shape declared in sem-api.d.ts -- name/type/parent_name/signature/doc are all missing or misplaced", async () => {
  await withTempDir(async (dir) => {
    writeFileSync(
      join(dir, "math.ts"),
      ["/** Adds two numbers. */", "export function add(a: number, b: number): number {", "  return a + b;", "}", ""].join("\n"),
      "utf8",
    );

    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = await api.headers([{ name: "add", file: "math.ts" }]);
    const lines = result as Array<Record<string, unknown>>;
    assert.ok(Array.isArray(lines) && lines.length === 1, `expected one HeaderLine, got: ${JSON.stringify(result)}`);
    const line = lines[0]!;

    // Every field sem-api.d.ts's HeaderLine declares, checked directly --
    // a script written against that contract expects all six to exist at
    // the TOP LEVEL with these exact types.
    assert.equal(typeof line.name, "string", `HeaderLine.name should be a top-level string; got ${JSON.stringify(line.name)} (actual shape: ${JSON.stringify(line)})`);
    assert.equal(typeof line.type, "string", `HeaderLine.type should be a top-level string; got ${JSON.stringify(line.type)}`);
    assert.equal(typeof line.file, "string", `HeaderLine.file should be a top-level string; got ${JSON.stringify(line.file)}`);
    assert.equal(typeof line.signature, "string", `HeaderLine.signature should be a top-level string with the entity's clean signature line; got ${JSON.stringify(line.signature)}`);
    assert.match(
      (line.signature as string) ?? "",
      /^export function add/,
      `HeaderLine.signature should be the entity's own declaration line; got ${JSON.stringify(line.signature)}`,
    );
  });
});
