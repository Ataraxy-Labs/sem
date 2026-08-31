import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { runCommand } from "./internal/proc.ts";
import { mapWithConcurrency } from "./internal/concurrency.ts";

export interface SemFindParams {
  /** Required unless `queries` is given. */
  query?: string;
  /** Look up several names in one call instead of one call per query. Same type=/limit= applied to every query. Takes priority over `query` when non-empty. */
  queries?: string[];
  type?: string;
  limit?: number;
}

export interface SemFindDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemFindOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

interface RawFindHit {
  id?: string;
  name: string;
  type: string;
  file: string;
  start_line: number;
  end_line: number;
}

const SemFindParamsSchema = Type.Object({
  query: Type.Optional(
    Type.String({
      description:
        "EXACT entity name, case-sensitive, no substring/fuzzy matching — e.g. \"add\" or \"greet\". Optionally prefix the kind to narrow it: \"method greet\", \"class Alice\", \"function add\". Required unless queries= is used.",
    }),
  ),
  queries: Type.Optional(
    Type.Array(Type.String(), {
      description: "Look up several names in one call instead of one call per query; same type=/limit= applied to each. Takes priority over query= when given. Capped at 25 per call — the rest are reported as not run.",
    }),
  ),
  type: Type.Optional(
    Type.String({
      description:
        "Entity kind to filter by, e.g. 'function', 'class', 'method', 'variable' (language-dependent; matches sem's `type` field). Shorthand for prefixing the query with the kind.",
    }),
  ),
  limit: Type.Optional(
    Type.Integer({
      minimum: 1,
      description: "Max results to show (default 20). More matches are summarized as a trailing '…N more' count.",
    }),
  ),
});

/** Default cap on rendered results, enforced client-side (the CLI has no --limit). */
const DEFAULT_LIMIT = 20;

/**
 * Hard cap on how many queries= entries run per call, and how many of them
 * run at once. Unbounded batching let a caller fire an arbitrary number of
 * `sem find` child processes in one call with no ceiling on either count —
 * a query past MAX_BATCH_QUERIES is reported as skipped rather than
 * silently dropped or silently run anyway (same "honest, not silent" cap
 * sem_read's entities= budget uses), and MAX_CONCURRENT_QUERIES bounds how
 * many of the accepted queries are actually in flight at once so the array
 * cap alone doesn't just relocate the same unbounded fan-out one level
 * down.
 */
const MAX_BATCH_QUERIES = 25;
const MAX_CONCURRENT_QUERIES = 4;

/**
 * Runs `sem find <query> --json` and parses its flat result array. The CLI
 * exits 0 both for matches and for an empty `[]`, so any exit code is accepted
 * as long as stdout parses as JSON; a crash surfaces as unparseable output.
 */
async function runSemFindJson(query: string, type: string | undefined, deps: SemFindDeps): Promise<RawFindHit[]> {
  const { cwd, semBin, signal } = deps;
  // sem has no --type flag: a kind-filtered query is spelled "type name".
  const fullQuery = type !== undefined ? `${type} ${query}` : query;

  const result = await runCommand(semBin, ["find", fullQuery, "--json"], cwd, signal);

  let hits: RawFindHit[];
  try {
    hits = JSON.parse(result.stdout) as RawFindHit[];
  } catch (err) {
    throw new Error(
      `"${semBin} find ${JSON.stringify(fullQuery)} --json" failed (exit ${result.exitCode}): ${
        result.stderr.trim() || (err instanceof Error ? err.message : String(err))
      }`,
    );
  }
  return Array.isArray(hits) ? hits : [];
}

/**
 * One query's full lookup, independent of the query= vs queries= dispatch
 * below. Zero matches is a success (a plain sentence, isError=false); only a
 * genuine execution error (sem crashed, binary missing, bad JSON) comes back
 * as isError=true.
 */
async function runOneQuery(query: string, params: SemFindParams, deps: SemFindDeps): Promise<SemFindOutcome> {
  const baseDetails = { query, type: params.type ?? null };

  let hits: RawFindHit[];
  try {
    hits = await runSemFindJson(query, params.type, deps);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_find: ${message}`,
      details: { ...baseDetails, error: message },
    };
  }

  const total = hits.length;
  if (total === 0) {
    const scope = params.type !== undefined ? ` of kind "${params.type}"` : "";
    return {
      isError: false,
      text: `sem_find: no entities${scope} named "${query}". sem's matching is exact and case-sensitive — check the spelling, or use sem_grep to search text instead of names.`,
      details: { ...baseDetails, total: 0, shown: 0, hits: [] },
    };
  }

  const limit = Math.max(1, params.limit ?? DEFAULT_LIMIT);
  const shown = hits.slice(0, limit);
  const omitted = total - shown.length;

  const lines = shown.map((h) => `${h.type} ${h.name} ${h.file}:L${h.start_line}-L${h.end_line}`);
  if (omitted > 0) lines.push(`…${omitted} more`);

  return {
    isError: false,
    text: lines.join("\n"),
    details: {
      ...baseDetails,
      total,
      shown: shown.length,
      hits: shown.map((h) => ({
        name: h.name,
        type: h.type,
        file: h.file,
        start_line: h.start_line,
        end_line: h.end_line,
      })),
    },
  };
}

/**
 * The tool's full orchestration, independent of pi's tool-registration glue
 * so it can be driven directly in tests. queries= (non-empty) runs every
 * query with the same type=/limit=, grouped in the result; otherwise the
 * single query= form returns exactly what it always has (aside from its
 * `results` field being renamed `hits`, matching sem_grep's naming) — no
 * batch wrapper, so existing single-query callers are unaffected.
 */
export async function performSemFind(params: SemFindParams, deps: SemFindDeps): Promise<SemFindOutcome> {
  const queries = params.queries;
  if (queries === undefined || queries.length === 0) {
    if (params.query === undefined) {
      return { isError: true, text: "sem_find: pass either query= or queries=[...].", details: { error: "missing query" } };
    }
    return runOneQuery(params.query, params, deps);
  }

  const accepted = queries.slice(0, MAX_BATCH_QUERIES);
  const omitted = queries.length - accepted.length;

  const perQuery = await mapWithConcurrency(accepted, MAX_CONCURRENT_QUERIES, (query) => runOneQuery(query, params, deps));

  const blocks = perQuery.map((r, i) => `query "${accepted[i]}":\n${r.text}`);
  if (omitted > 0) {
    blocks.push(`…${omitted} more not run (queries= is capped at ${MAX_BATCH_QUERIES} per call)`);
  }
  const allFailed = perQuery.length > 0 && perQuery.every((r) => r.isError);

  return {
    isError: allFailed,
    text: blocks.join("\n\n"),
    details: {
      total_queries: queries.length,
      ran: accepted.length,
      omitted,
      results: perQuery.map((r, i) => ({ query: accepted[i], ...r.details })),
    },
  };
}

export interface RegisterSemFindOptions {
  /** `sem` binary to shell out to for name lookup. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_find` tool. Call once per pi extension load. */
export function registerSemFind(pi: ExtensionAPI, opts: RegisterSemFindOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_find",
    label: "Sem Find",
    description:
      "Find where named entities are defined (exact, case-sensitive, index-backed, <10ms). Pass queries=[...] to batch several lookups in one call.",
    promptSnippet: "Locate a named function/class/method's definition(s) by exact name, with file and line range",
    promptGuidelines: [
      "Reach for sem_find FIRST when you don't know where something lives in the repo — it answers 'where is <name> defined' from an index instead of you guessing a file path.",
      "Pass an exact, case-sensitive name; there is NO substring or fuzzy matching ('gree' will not find 'greet'). When unsure of the exact spelling, use sem_grep on a fragment instead.",
      "Narrow by kind with type= (e.g. type=\"method\") when the name exists as more than one kind of entity.",
    ],
    parameters: SemFindParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemFind(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
