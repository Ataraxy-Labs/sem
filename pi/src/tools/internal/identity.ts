import { extname } from "node:path";

/**
 * `sem entities --json` exposes no visibility/export field (confirmed
 * empirically — an exported and a non-exported TS function produce
 * identical JSON shapes). Visibility here is derived from a small
 * per-language heuristic instead: TS/JS `export`/`export default` prefixes
 * on the entity's own source line, Rust `pub`/`pub(...)` prefixes, Go's
 * capitalized-identifier convention, Python's underscore-prefix convention.
 *
 * This is deliberately narrow and will not catch every visibility
 * mechanism a language offers (`export =`, Rust `pub use`, Python
 * `__all__`, ...) — "unknown" is the honest answer for anything outside
 * that, EXCEPT TS/JS named-export lists (`export { x }` / `export { x as
 * y }`) and a bare `export default x;`, which deriveVisibility scans the
 * whole file for explicitly (see hasNamedExport below), since without that
 * a same-file style refactor — moving `export` off the declaration onto a
 * separate list further down — reads as a genuine visibility change when
 * nothing about the entity's actual exported-ness changed at all. Callers
 * must never treat "unknown" as a detected change, only as "no opinion".
 */
export type Visibility = "exported" | "not-exported" | "unknown";

/**
 * hasNamedExport used to regex-scan the file's raw
 * text with no comment/string stripping, so a comment or string literal
 * merely MENTIONING `export { name }` (a migration note, a code example in
 * a doc comment, ...) counted as proof the entity is genuinely exported —
 * reopening exactly the class of live bug (fb44a78, an `export` drop
 * reaching disk undetected) this whole identity check exists to catch.
 *
 * Replaces every `//` line comment, `/* *\/` block comment, and
 * `'...'`/`"..."`/`` `...` `` string/template literal with same-length
 * whitespace (so any offsets a caller might still care about are
 * preserved) before scanning. Deliberately a small hand-rolled scanner, not
 * a real lexer — same "honest heuristic, not a parser" spirit as
 * deriveVisibility itself.
 *
 * The pass-3 version had zero regex-literal
 * awareness — a genuine regex like `/don't match/` was copied character by
 * character, and when the scan reached the apostrophe INSIDE it, the
 * quote-matching branch below mistook it for the start of a NEW
 * single-quoted string. With no other apostrophe later in the file to
 * "close" it, that accidental string ran all the way to EOF, blanking
 * everything after it — including a genuine, untouched `export { name };`
 * further down. That's a false NEGATIVE with real consequences (unlike the
 * template-literal-interpolation gap noted below, which only ever risks a
 * false negative too, but confined to the template itself): it could hide
 * a real export-list statement from `hasNamedExport` for reasons entirely
 * unrelated to the entity actually being edited.
 *
 * Two fixes, both defense in depth:
 * (1) `isRegexLiteralPosition` below recognizes when a `/` opens a regex
 *     literal rather than a division operator (see its own doc comment for
 *     the exact rule) and skips to its closing unescaped `/`, respecting
 *     `[...]` character classes the same way a real lexer would.
 * (2) A `'`/`"` string scan (never a `` ` `` template, which legitimately
 *     spans lines) that reaches a newline before finding its closing quote
 *     stops AT that newline instead of continuing — so even a scan that
 *     mis-starts for some OTHER reason this file hasn't anticipated yet
 *     costs at most one line, never the rest of the file.
 *
 * `${...}` interpolation inside a template literal is still unhandled (the
 * whole template, interpolations included, is blanked) — deliberately
 * left as the one remaining gap, since it only risks a false negative
 * confined to content already inside the template literal itself, not an
 * unbounded blast radius the way the regex-literal gap was.
 */
function stripCommentsAndStrings(source: string): string {
  let result = "";
  let i = 0;
  while (i < source.length) {
    const two = source.slice(i, i + 2);
    if (two === "//") {
      const newlineIdx = source.indexOf("\n", i);
      const stop = newlineIdx === -1 ? source.length : newlineIdx;
      result += " ".repeat(stop - i);
      i = stop;
      continue;
    }
    if (two === "/*") {
      const endIdx = source.indexOf("*/", i + 2);
      const stop = endIdx === -1 ? source.length : endIdx + 2;
      result += source.slice(i, stop).replace(/[^\n]/g, " ");
      i = stop;
      continue;
    }
    const ch = source[i]!;
    if (ch === "/" && isRegexLiteralPosition(result)) {
      let j = i + 1;
      let inClass = false;
      while (j < source.length) {
        const cj = source[j];
        if (cj === "\\") {
          j += 2;
          continue;
        }
        if (cj === "\n") break; // a regex literal can't span a raw newline; bail rather than eat the rest of the file
        if (cj === "[") {
          inClass = true;
          j++;
          continue;
        }
        if (cj === "]") {
          inClass = false;
          j++;
          continue;
        }
        if (cj === "/" && !inClass) {
          j++;
          break;
        }
        j++;
      }
      result += source.slice(i, j).replace(/[^\n]/g, " ");
      i = j;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === "`") {
      const boundToLine = ch !== "`"; // `'`/`"` are always single-line in valid source; `` ` `` legitimately spans lines
      let j = i + 1;
      while (j < source.length) {
        if (source[j] === "\\") {
          j += 2;
          continue;
        }
        if (source[j] === ch) {
          j++;
          break;
        }
        if (boundToLine && source[j] === "\n") break; // defense in depth: a mis-opened string costs one line, not the file
        j++;
      }
      result += source.slice(i, j).replace(/[^\n]/g, " ");
      i = j;
      continue;
    }
    result += ch;
    i++;
  }
  return result;
}

/**
 * Whether a `/` at the current scan position opens a regex literal rather
 * than being a division/compound-assignment operator — decided from the
 * last non-whitespace character already emitted into `result` (the
 * comment/string-stripped output so far, which is exactly what a real
 * lexer would have available at this point too).
 *
 * Not division (regex position) when that character is `)`, `]`, or an
 * identifier/number character (`[A-Za-z0-9_$]`) — EXCEPT when the word
 * ending there is a keyword that itself ends in a word character but still
 * expects an expression next (`return`, `typeof`, `case`, `in`, `of`,
 * `new`, `delete`, `void`, `throw`, `instanceof`, `yield`, `do`, `else`,
 * `await`, `default`) — `return /foo/` must still open a regex, not divide
 * `return` by `foo`. Anything else (`(`, `,`, `=`, `:`, `;`, `{`, an
 * operator, or the very start of the scan) is regex position.
 */
function isRegexLiteralPosition(result: string): boolean {
  let k = result.length - 1;
  while (k >= 0 && /\s/.test(result[k]!)) k--;
  if (k < 0) return true;

  const c = result[k]!;
  if (/[A-Za-z0-9_$]/.test(c)) {
    let start = k;
    while (start >= 0 && /[A-Za-z0-9_$]/.test(result[start]!)) start--;
    const word = result.slice(start + 1, k + 1);
    const EXPRESSION_KEYWORDS = new Set([
      "return",
      "typeof",
      "case",
      "in",
      "of",
      "new",
      "delete",
      "void",
      "throw",
      "instanceof",
      "yield",
      "do",
      "else",
      "await",
      "default",
    ]);
    return EXPRESSION_KEYWORDS.has(word);
  }
  if (c === ")" || c === "]") return false;
  return true;
}

/**
 * Whether `entityName` is exported via a named-export list (`export { x }`,
 * `export { x as y }` — matched on the LOCAL binding name, before any
 * `as`) or a standalone `export default x;` statement anywhere in the
 * file, independent of its own declaration line. Scans comment/string-
 * stripped text (see stripCommentsAndStrings) so a mention inside a
 * comment or string literal never counts.
 */
function hasNamedExport(fileContent: string, entityName: string): boolean {
  const codeOnly = stripCommentsAndStrings(fileContent);
  const exportBlockRe = /export\s*\{([^}]*)\}/g;
  let match: RegExpExecArray | null;
  while ((match = exportBlockRe.exec(codeOnly)) !== null) {
    const entries = match[1]!.split(",").map((entry) => entry.trim()).filter((entry) => entry.length > 0);
    for (const entry of entries) {
      const localName = entry.split(/\s+as\s+/)[0]!.trim();
      if (localName === entityName) return true;
    }
  }
  const escaped = entityName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`export\\s+default\\s+${escaped}\\s*;`).test(codeOnly);
}

function languageOf(filePath: string): "ts" | "js" | "rust" | "python" | "go" | "other" {
  switch (extname(filePath)) {
    case ".ts":
    case ".tsx":
    case ".mts":
    case ".cts":
      return "ts";
    case ".js":
    case ".jsx":
    case ".mjs":
    case ".cjs":
      return "js";
    case ".rs":
      return "rust";
    case ".py":
      return "python";
    case ".go":
      return "go";
    default:
      return "other";
  }
}

/**
 * `fileContent`, when given, is scanned for a named-export list or bare
 * `export default` covering `entityName` before falling back to
 * "not-exported" for TS/JS — omit it (or pass undefined) to fall back to
 * the old own-line-only heuristic.
 */
export function deriveVisibility(filePath: string, entityName: string, sourceLine: string, fileContent?: string): Visibility {
  switch (languageOf(filePath)) {
    case "ts":
    case "js": {
      if (/^\s*export\s+(default\s+)?/.test(sourceLine)) return "exported";
      if (fileContent !== undefined && hasNamedExport(fileContent, entityName)) return "exported";
      return "not-exported";
    }
    case "rust":
      return /^\s*pub(\([^)]*\))?\s+/.test(sourceLine) ? "exported" : "not-exported";
    case "go":
      return /^[A-Z]/.test(entityName) ? "exported" : "not-exported";
    case "python":
      return entityName.startsWith("_") ? "not-exported" : "exported";
    default:
      return "unknown";
  }
}

export interface IdentityFacts {
  name: string;
  type: string;
  parentName: string | null;
  visibility: Visibility;
}

export interface IdentityChange {
  field: "name" | "entity_type" | "parent" | "visibility";
  before: string;
  after: string;
}

/**
 * Compares the identity-level facts of a replaced entity before/after.
 * `after: undefined` means re-extraction found no entity at the edited
 * location at all — reported as a change, never silently ignored (the
 * existing vanished-entity check usually catches this case first anyway,
 * but this is a direct, specific signal for exactly this entity).
 */
export function compareIdentity(before: IdentityFacts, after: IdentityFacts | undefined): IdentityChange[] {
  if (!after) {
    return [{ field: "name", before: before.name, after: "(no entity found at the edited location)" }];
  }

  const changes: IdentityChange[] = [];
  if (before.name !== after.name) changes.push({ field: "name", before: before.name, after: after.name });
  if (before.type !== after.type) changes.push({ field: "entity_type", before: before.type, after: after.type });
  if (before.parentName !== after.parentName) {
    changes.push({ field: "parent", before: before.parentName ?? "(none)", after: after.parentName ?? "(none)" });
  }
  if (before.visibility !== "unknown" && after.visibility !== "unknown" && before.visibility !== after.visibility) {
    changes.push({ field: "visibility", before: before.visibility, after: after.visibility });
  }
  return changes;
}
