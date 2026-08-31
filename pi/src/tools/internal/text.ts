/**
 * Line-level text splicing for entity edits: detects the file's EOL style
 * and trailing-newline convention, re-indents inserted/replacing content to
 * match the surrounding code, and reassembles the file without disturbing
 * anything outside the touched range.
 */

export type Op = "replace" | "insert_after" | "insert_before" | "delete";

export interface LineModel {
  eol: "\n" | "\r\n";
  hasTrailingNewline: boolean;
  lines: string[];
}

export function parseLines(content: string): LineModel {
  const crlfCount = (content.match(/\r\n/g) ?? []).length;
  const bareLfCount = (content.match(/(?<!\r)\n/g) ?? []).length;
  const eol: "\n" | "\r\n" = crlfCount > bareLfCount ? "\r\n" : "\n";

  const normalized = content.replace(/\r\n/g, "\n");
  const hasTrailingNewline = normalized.length > 0 && normalized.endsWith("\n");
  const body = hasTrailingNewline ? normalized.slice(0, -1) : normalized;
  const lines = body.length === 0 ? [] : body.split("\n");

  return { eol, hasTrailingNewline, lines };
}

export function renderLines(model: Pick<LineModel, "eol" | "hasTrailingNewline">, lines: string[]): string {
  const body = lines.join(model.eol);
  return lines.length > 0 && model.hasTrailingNewline ? body + model.eol : body;
}

function leadingWhitespace(line: string): string {
  return line.match(/^[ \t]*/)?.[0] ?? "";
}

function commonPrefix(a: string, b: string): string {
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  return a.slice(0, i);
}

function dedent(lines: string[]): string[] {
  const nonBlank = lines.filter((l) => l.trim().length > 0);
  if (nonBlank.length === 0) return lines;
  const common = nonBlank
    .map(leadingWhitespace)
    .reduce((acc, ws) => commonPrefix(acc, ws));
  if (common.length === 0) return lines;
  return lines.map((l) => (l.startsWith(common) ? l.slice(common.length) : l.replace(/^[ \t]*/, "")));
}

/** Strips whatever common indentation `contentLines` already has, then applies `indent` uniformly. */
export function reindent(contentLines: string[], indent: string): string[] {
  return dedent(contentLines).map((l) => (l.trim().length === 0 ? l : indent + l));
}

function isBlank(line: string | undefined): boolean {
  return line === undefined || line.trim().length === 0;
}

function countTrailingBlank(lines: string[]): number {
  let n = 0;
  while (n < lines.length && isBlank(lines[lines.length - 1 - n])) n++;
  return n;
}

export interface SpliceResult {
  text: string;
  /** 1-indexed inclusive range the edited/inserted content now occupies; undefined for a delete. */
  newRange: { start: number; end: number } | undefined;
  /** The exact (re-indented) text placed at newRange; undefined for a delete. */
  insertedText: string | undefined;
}

export function splice(original: string, entityRange: { start_line: number; end_line: number }, op: Op, content: string | undefined): SpliceResult {
  const model = parseLines(original);
  const before = model.lines.slice(0, entityRange.start_line - 1);
  const target = model.lines.slice(entityRange.start_line - 1, entityRange.end_line);
  const after = model.lines.slice(entityRange.end_line);
  const anchorIndent = leadingWhitespace(model.lines[entityRange.start_line - 1] ?? "");

  if (op === "delete") {
    const collapseDoubleBlank = isBlank(before[before.length - 1]) && isBlank(after[0]);
    const merged = collapseDoubleBlank ? [...before, ...after.slice(1)] : [...before, ...after];
    return { text: renderLines(model, merged), newRange: undefined, insertedText: undefined };
  }

  const contentLines = content !== undefined ? content.split(/\r\n|\n/) : [];
  const reindented = reindent(contentLines, anchorIndent);
  const insertedText = reindented.join(model.eol);

  if (op === "replace") {
    // The entity's own range can include a trailing blank separator line
    // (sem reports a markdown heading's end_line through the blank line
    // before the next heading, for example) — that line lives in `target`,
    // not `after`, so a plain concat silently drops it whenever the
    // replacement content doesn't itself end with a blank line.
    const targetTrailingBlanks = countTrailingBlank(target);
    const paddedReindented =
      targetTrailingBlanks > 0 && countTrailingBlank(reindented) === 0
        ? [...reindented, ...Array<string>(targetTrailingBlanks).fill("")]
        : reindented;
    const merged = [...before, ...paddedReindented, ...after];
    // sem reports the padded blank line as part of the entity's own range
    // both before and after the edit (see countTrailingBlank above) — it's
    // restored entity content, not a tool-inserted separator, so newRange
    // and insertedText (sent to weave-mcp as this entity's new content)
    // must include it too, matching what's actually written to `text`.
    return {
      text: renderLines(model, merged),
      newRange: { start: before.length + 1, end: before.length + paddedReindented.length },
      insertedText: paddedReindented.join(model.eol),
    };
  }

  const anchorBefore = op === "insert_after" ? [...before, ...target] : before;
  const anchorAfter = op === "insert_after" ? after : [...target, ...after];

  const sepBefore = !isBlank(anchorBefore[anchorBefore.length - 1]) && !isBlank(reindented[0]) && anchorBefore.length > 0;
  const sepAfter = !isBlank(anchorAfter[0]) && !isBlank(reindented[reindented.length - 1]) && anchorAfter.length > 0;

  const pre = sepBefore ? [...anchorBefore, ""] : anchorBefore;
  const post = sepAfter ? ["", ...anchorAfter] : anchorAfter;
  const merged = [...pre, ...reindented, ...post];

  return {
    text: renderLines(model, merged),
    newRange: { start: pre.length + 1, end: pre.length + reindented.length },
    insertedText,
  };
}
