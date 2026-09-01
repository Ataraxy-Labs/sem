import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Type } from "typebox";
import { runCommand } from "./internal/proc.ts";
import { mapWithConcurrency } from "./internal/concurrency.ts";

export interface SemGrepParams {
  /** Required unless `patterns` is given. */
  pattern?: string;
  /** Search several patterns in one call instead of one call per pattern. Same path=/glob=/context=/limit= applied to every pattern. Takes priority over `pattern` when non-empty. */
  patterns?: string[];
  path?: string;
  glob?: string;
  context?: number;
  limit?: number;
  /** Treat every pattern as literal text rather than a regex -- see escapeRegexLiteral. */
  literal?: boolean;
}

export interface SemGrepDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemGrepOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

interface RawGrepHit {
  file: string;
  line: number;
  text: string;
}

interface RawGrepOutput {
  hits: RawGrepHit[];
}

const SemGrepParamsSchema = Type.Object({
  pattern: Type.Optional(
    Type.String({
      description:
        "Regex to search for (rg-compatible). Served from a trigram index, so literal-ish patterns are fastest; unconstrained regexes degrade to an honest full scan. Required unless patterns= is used.",
    }),
  ),
  patterns: Type.Optional(
    Type.Array(Type.String(), {
      description: "Search several patterns in one call instead of one call per pattern; same path=/glob=/context=/limit= applied to each. Takes priority over pattern= when given. Capped at 25 per call — the rest are reported as not run.",
    }),
  ),
  path: Type.Optional(
    Type.String({
      description: "Restrict hits to this file or directory prefix (repo-relative), e.g. \"src\" or \"src/tools/sem-find.ts\".",
    }),
  ),
  glob: Type.Optional(
    Type.String({
      description: "Restrict hits to files matching this glob, e.g. \"*.ts\" or \"subdir/**\". * matches within a path segment, ** across segments, ? one character.",
    }),
  ),
  context: Type.Optional(
    Type.Integer({
      minimum: 0,
      description: "Lines of surrounding source to show before and after each hit, each numbered, the matched line marked with '>'. Default 0 (hit line only).",
    }),
  ),
  limit: Type.Optional(
    Type.Integer({
      minimum: 1,
      description: "Max hits to show (default 20). More matches are summarized as a trailing '…N more' count.",
    }),
  ),
  literal: Type.Optional(
    Type.Boolean({
      description:
        "Search for the pattern as LITERAL TEXT instead of a regex. Use it whenever the pattern is code -- \"is_fits(\", \"only(\", \"col_suffixes=['\" -- since parens and brackets are regex syntax and an unbalanced one is a parse error, not a search.",
    }),
  ),
});

/**
 * P7 of the 2026-09-02 transcript study: 170 regex parse errors across 117
 * of 327 runs (36%), 25 of them fatal to the whole script, all from agents
 * typing CODE into a regex-only search -- `is_fits(`, `only(`, `Prefetch(`,
 * `col_suffixes=['`.
 *
 * Escapes exactly the characters Rust's own `regex::escape` treats as meta
 * (`\ . + * ? ( ) | [ ] { } ^ $ # & - ~`) and nothing else, because sem's
 * pattern is a Rust regex. Escaping MORE would be actively wrong: `\<` and
 * `\>` are word-boundary assertions in that dialect, so backslashing every
 * punctuation character would silently change what a literal `<` matches.
 */
export function escapeRegexLiteral(pattern: string): string {
  return pattern.replace(/[\\.+*?()|[\]{}^$#&~-]/g, (c) => `\\${c}`);
}

/** The one place a pattern is chosen: what actually goes to `sem grep`. `details.pattern` always echoes what the CALLER typed, never this. */
function searchPattern(pattern: string, literal: boolean | undefined): string {
  return literal === true ? escapeRegexLiteral(pattern) : pattern;
}

/** Default cap on rendered hits, enforced client-side (the CLI has no --limit). */
const DEFAULT_LIMIT = 20;

/**
 * Same batch-boundary guards as sem-find.ts's MAX_BATCH_QUERIES/
 * MAX_CONCURRENT_QUERIES, for patterns=: an honest cap on how many patterns
 * run per call (a pattern past the cap is reported as skipped, matching
 * sem_read's entities= budget-cap convention) and a concurrency ceiling on
 * how many run at once, so the cap alone doesn't just relocate the same
 * unbounded fan-out one level down.
 */
const MAX_BATCH_PATTERNS = 25;
const MAX_CONCURRENT_PATTERNS = 4;

/**
 * Small dependency-free glob-to-regex translator for client-side file
 * filtering: `**` = any chars including `/`, `*` = any chars except `/`,
 * `?` = exactly one char; every other regex-special character is escaped.
 */
function globToRegex(glob: string): RegExp {
  let source = "";
  for (let i = 0; i < glob.length; i++) {
    const char = glob[i]!;
    if (char === "*" && glob[i + 1] === "*") {
      source += ".*";
      i++;
    } else if (char === "*") {
      source += "[^/]*";
    } else if (char === "?") {
      source += ".";
    } else {
      source += char.replace(/[.+^${}()|[\]\\]/g, "\\$&");
    }
  }
  return new RegExp(`^${source}$`);
}

function matchesPathFilter(file: string, path: string): boolean {
  return file === path || file.startsWith(`${path}/`);
}

/**
 * Runs `sem grep <pattern> --json` and returns its hits. The CLI exits 0 with
 * matches and exit 1 with zero of them (`hits: []` — rg's no-match convention,
 * NOT an error), so any exit code is accepted as long as stdout parses as
 * JSON; only unparseable output (crash, invalid regex → exit 2) throws.
 */
async function runSemGrepJson(pattern: string, deps: SemGrepDeps): Promise<RawGrepHit[]> {
  const result = await runCommand(deps.semBin, ["grep", pattern, "--json"], deps.cwd, deps.signal);

  let parsed: RawGrepOutput;
  try {
    parsed = JSON.parse(result.stdout) as RawGrepOutput;
  } catch (err) {
    throw new Error(
      `"${deps.semBin} grep ${JSON.stringify(pattern)} --json" failed (exit ${result.exitCode}): ${
        result.stderr.trim() || (err instanceof Error ? err.message : String(err))
      }`,
    );
  }
  return Array.isArray(parsed?.hits) ? parsed.hits : [];
}

/** Renders one hit as the plain single line used when context is off. */
function plainHitLine(hit: RawGrepHit): string {
  return `${hit.file}:L${hit.line}: ${hit.text}`;
}

/**
 * Renders one hit as a small block: the plain `file:Lline: text` form, then
 * `context` numbered lines on each side, the matched line marked with '>'.
 * Falls back to the plain line alone if the file can't be read — a missing
 * file is degraded output, not a tool failure.
 */
async function contextHitBlock(hit: RawGrepHit, context: number, readCached: (file: string) => Promise<string | undefined>): Promise<string[]> {
  const content = await readCached(hit.file);
  if (content === undefined) return [plainHitLine(hit)];

  const lines = content.split("\n");
  if (lines.length > 0 && lines[lines.length - 1] === "") lines.pop();

  const first = Math.max(1, hit.line - context);
  const last = Math.min(lines.length, hit.line + context);

  const block = [plainHitLine(hit)];
  for (let n = first; n <= last; n++) {
    const marker = n === hit.line ? ">" : " ";
    block.push(`${marker} ${n}| ${lines[n - 1] ?? ""}`);
  }
  return block;
}

/**
 * One pattern's full search, independent of the pattern= vs patterns=
 * dispatch below — path=/glob= filter client-side, then limit= caps what's
 * rendered. Zero matches is a success (a plain sentence, isError=false);
 * only a genuine execution error comes back isError=true.
 */
async function runOnePattern(pattern: string, params: SemGrepParams, deps: SemGrepDeps): Promise<SemGrepOutcome> {
  const literal = params.literal === true;
  const baseDetails = {
    pattern,
    literal,
    path: params.path ?? null,
    glob: params.glob ?? null,
    context: params.context ?? 0,
  };

  let rawHits: RawGrepHit[];
  try {
    rawHits = await runSemGrepJson(searchPattern(pattern, params.literal), deps);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    // P7: the fix for a parse error is almost always "I meant that as
    // text" -- say so IN the error, so one wall-hit teaches the way over
    // it instead of costing a retry (or, 25 times in the drive, the whole
    // script).
    const hint =
      !literal && /regex parse error|unclosed|unrecognized|repetition|unmatched|invalid pattern/i.test(message)
        ? ` If you meant this as literal text (identifiers and call sites contain ( [ { and other regex syntax), pass { literal: true }.`
        : "";
    return {
      isError: true,
      text: `sem_grep: ${message}${hint}`,
      details: { ...baseDetails, error: message },
    };
  }

  const globRe = params.glob !== undefined ? globToRegex(params.glob) : undefined;
  const filtered = rawHits.filter((hit) => {
    if (params.path !== undefined && !matchesPathFilter(hit.file, params.path)) return false;
    if (globRe !== undefined && !globRe.test(hit.file)) return false;
    return true;
  });

  const total = filtered.length;
  if (total === 0) {
    return {
      isError: false,
      text: `sem_grep: no match for /${pattern}/${params.path !== undefined ? ` under ${params.path}` : ""}.`,
      details: { ...baseDetails, total: 0, shown: 0, hits: [] },
    };
  }

  // Per-file reads are shared across all hits in the same file so overlapping
  // context windows don't re-read it once per hit.
  const fileCache = new Map<string, string>();
  const readCached = async (file: string): Promise<string | undefined> => {
    const cached = fileCache.get(file);
    if (cached !== undefined) return cached;
    try {
      const content = await readFile(resolve(deps.cwd, file), "utf8");
      fileCache.set(file, content);
      return content;
    } catch {
      return undefined;
    }
  };

  const limit = Math.max(1, params.limit ?? DEFAULT_LIMIT);
  const shown = filtered.slice(0, limit);
  const omitted = total - shown.length;

  const context = params.context ?? 0;
  const lines: string[] = [];
  for (const hit of shown) {
    if (context > 0) lines.push(...(await contextHitBlock(hit, context, readCached)));
    else lines.push(plainHitLine(hit));
  }
  if (omitted > 0) lines.push(`…${omitted} more`);

  return {
    isError: false,
    text: lines.join("\n"),
    details: {
      ...baseDetails,
      total,
      shown: shown.length,
      hits: shown.map((h) => ({ file: h.file, line: h.line, text: h.text })),
    },
  };
}

/**
 * The tool's full orchestration, independent of pi's tool-registration glue
 * so it can be driven directly in tests. patterns= (non-empty) runs every
 * pattern with the same path=/glob=/context=/limit=, grouped in the result;
 * otherwise the single pattern= form returns exactly what it always has —
 * no batch wrapper, so existing single-pattern callers are unaffected.
 */
export async function performSemGrep(params: SemGrepParams, deps: SemGrepDeps): Promise<SemGrepOutcome> {
  const patterns = params.patterns;
  if (patterns === undefined || patterns.length === 0) {
    if (params.pattern === undefined) {
      return { isError: true, text: "sem_grep: pass either pattern= or patterns=[...].", details: { error: "missing pattern" } };
    }
    return runOnePattern(params.pattern, params, deps);
  }

  const accepted = patterns.slice(0, MAX_BATCH_PATTERNS);
  const omitted = patterns.length - accepted.length;

  const perPattern = await mapWithConcurrency(accepted, MAX_CONCURRENT_PATTERNS, (pattern) => runOnePattern(pattern, params, deps));

  const blocks = perPattern.map((r, i) => `pattern "${accepted[i]}":\n${r.text}`);
  if (omitted > 0) {
    blocks.push(`…${omitted} more not run (patterns= is capped at ${MAX_BATCH_PATTERNS} per call)`);
  }
  const allFailed = perPattern.length > 0 && perPattern.every((r) => r.isError);

  return {
    isError: allFailed,
    text: blocks.join("\n\n"),
    details: {
      total_patterns: patterns.length,
      ran: accepted.length,
      omitted,
      results: perPattern.map((r, i) => ({ pattern: accepted[i], ...r.details })),
    },
  };
}

export interface RegisterSemGrepOptions {
  /** `sem` binary to shell out to for text search. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_grep` tool. Call once per pi extension load. */
export function registerSemGrep(pi: ExtensionAPI, opts: RegisterSemGrepOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_grep",
    label: "Sem Grep",
    description:
      "Search repo text (trigram-indexed): file:line hits for call sites, string literals, log/error messages. Regex, or literal=true for code. patterns=[...] batches.",
    promptSnippet: "Regex-search repo text for a pattern, getting file:line hits across all files",
    promptGuidelines: [
      "Reach for sem_grep FIRST when you don't know where something lives in the repo and there isn't a precise name to hand sem_find — e.g. call sites, string literals, log/error messages.",
      "Narrow big searches with path= (file or directory prefix) or glob= (e.g. \"*.ts\", \"subdir/**\") instead of paging through capped results.",
      "Pass context= (a few lines) to see each hit's surroundings before deciding what to open; raise limit= above the default 20 only when you truly need more.",
      "Searching for a snippet of code rather than a pattern? Pass literal=true -- \"is_fits(\" or \"col_suffixes=['\" is a regex parse error otherwise, not a search.",
    ],
    parameters: SemGrepParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemGrep(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
