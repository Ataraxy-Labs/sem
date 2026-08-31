import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { runCommand } from "./internal/proc.ts";

/**
 * Every call/reference site of a named entity, over `sem callers <query>
 * --json` — the index's reverse postings, same freshness/fallback
 * discipline as `sem find` (cheap, index-backed).
 *
 * The installed CLI (sem 0.23.0) has no --limit flag and never refuses on
 * an ambiguous name — it just returns one {entity, related} group per
 * matching entity in the JSON array. This tool enforces limit= itself
 * (matching sem_find/sem_grep's client-side-cap convention) and refuses
 * with a candidate list whenever more than one group comes back, rather
 * than silently picking one — the CLI may grow its own --limit/ambiguity
 * handling later; both are handled defensively here regardless.
 */

export interface SemCallersParams {
  name: string;
  entity_type?: string;
  limit?: number;
}

export interface SemCallersDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemCallersOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

interface RawEntityRef {
  id?: string;
  name: string;
  type: string;
  file: string;
  start_line: number;
  end_line: number;
}

interface RawCallersGroup {
  entity: RawEntityRef;
  related: RawEntityRef[];
}

const SemCallersParamsSchema = Type.Object({
  name: Type.String({
    description: "EXACT entity name, case-sensitive, no substring/fuzzy matching — e.g. \"add\" or \"greet\".",
  }),
  entity_type: Type.Optional(
    Type.String({
      description: "Entity kind to narrow by, e.g. 'function', 'class', 'method' (language-dependent; matches sem's `type` field).",
    }),
  ),
  limit: Type.Optional(
    Type.Integer({
      minimum: 1,
      description: "Max call sites to show (default 50). More are summarized as a trailing '…N more' count.",
    }),
  ),
});

/** Default cap on rendered call sites, enforced client-side (the installed CLI has no --limit). */
const DEFAULT_LIMIT = 50;

/**
 * Runs `sem callers <query> --json`. The CLI exits 0 both for matches and
 * for an empty `[]` (no entity by that name at all), so any exit code is
 * accepted as long as stdout parses as JSON; a crash surfaces as
 * unparseable output.
 */
async function runSemCallersJson(query: string, deps: SemCallersDeps): Promise<RawCallersGroup[]> {
  const result = await runCommand(deps.semBin, ["callers", query, "--json"], deps.cwd, deps.signal);
  let groups: RawCallersGroup[];
  try {
    groups = JSON.parse(result.stdout) as RawCallersGroup[];
  } catch (err) {
    throw new Error(
      `"${deps.semBin} callers ${JSON.stringify(query)} --json" failed (exit ${result.exitCode}): ${
        result.stderr.trim() || (err instanceof Error ? err.message : String(err))
      }`,
    );
  }
  return Array.isArray(groups) ? groups : [];
}

/** sem's entity ids are parent_id::name, recursively — the segment before the final name segment is always the immediate parent's own name (3-segment file::type::name ids have no parent). */
function parseParentNameFromId(id: string | undefined): string | null {
  if (id === undefined) return null;
  const parts = id.split("::");
  return parts.length > 3 ? (parts[parts.length - 2] ?? null) : null;
}

function formatCandidate(entity: RawEntityRef): string {
  const parentName = parseParentNameFromId(entity.id);
  const parent = parentName ? ` in ${parentName}` : "";
  return `  - ${entity.name} (${entity.type}${parent}) — ${entity.file}:L${entity.start_line}-L${entity.end_line}`;
}

/**
 * The tool's full orchestration, independent of pi's tool-registration glue
 * so it can be driven directly in tests. Zero callers is a success (a plain
 * sentence, isError=false); not-found and ambiguous are refusals
 * (isError=true); only a genuine execution error also comes back
 * isError=true.
 */
export async function performSemCallers(params: SemCallersParams, deps: SemCallersDeps): Promise<SemCallersOutcome> {
  const baseDetails = { name: params.name, entity_type: params.entity_type ?? null };
  const query = params.entity_type !== undefined ? `${params.entity_type} ${params.name}` : params.name;

  let groups: RawCallersGroup[];
  try {
    groups = await runSemCallersJson(query, deps);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_callers: ${message}`,
      details: { ...baseDetails, error: message },
    };
  }

  if (groups.length === 0) {
    return {
      isError: true,
      text: `sem_callers: no entity named "${params.name}" found. sem's matching is exact and case-sensitive — check the spelling, or use sem_find to confirm it exists.`,
      details: { ...baseDetails, resolved: false },
    };
  }

  if (groups.length > 1) {
    const list = groups.map((g) => formatCandidate(g.entity)).join("\n");
    return {
      isError: true,
      text: `sem_callers: "${params.name}" is ambiguous — ${groups.length} matches. Add entity_type to narrow it:\n${list}`,
      details: {
        ...baseDetails,
        resolved: false,
        candidates: groups.map((g) => ({
          name: g.entity.name,
          type: g.entity.type,
          file: g.entity.file,
          parent_name: parseParentNameFromId(g.entity.id),
          start_line: g.entity.start_line,
          end_line: g.entity.end_line,
        })),
      },
    };
  }

  const entity = groups[0]!.entity;
  const callers = groups[0]!.related;
  const total = callers.length;

  if (total === 0) {
    return {
      isError: false,
      text: `sem_callers: no callers found for "${entity.name}" (${entity.type}) in ${entity.file} — safe to rename/remove without updating call sites (per sem's syntax-level graph).`,
      details: { ...baseDetails, entity: { name: entity.name, type: entity.type, file: entity.file }, total: 0, shown: 0, callers: [] },
    };
  }

  const limit = Math.max(1, params.limit ?? DEFAULT_LIMIT);
  const shown = callers.slice(0, limit);
  const omitted = total - shown.length;

  const lines = shown.map((c) => `${c.file}:${c.start_line}  in ${c.type} ${c.name}`);
  if (omitted > 0) lines.push(`…${omitted} more`);

  return {
    isError: false,
    text: lines.join("\n"),
    details: {
      ...baseDetails,
      entity: { name: entity.name, type: entity.type, file: entity.file },
      total,
      shown: shown.length,
      callers: shown.map((c) => ({ name: c.name, type: c.type, file: c.file, start_line: c.start_line, end_line: c.end_line })),
    },
  };
}

export interface RegisterSemCallersOptions {
  /** `sem` binary to shell out to for reverse-reference lookup. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_callers` tool. Call once per pi extension load. */
export function registerSemCallers(pi: ExtensionAPI, opts: RegisterSemCallersOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_callers",
    label: "Sem Callers",
    description: "Every call/reference site of a named entity — cheap, index-backed; use before renaming or changing a signature.",
    promptSnippet: "List every call/reference site of a named entity, by exact name",
    promptGuidelines: [
      "Use sem_callers before renaming or changing a function/method/class's signature — it lists every call site so you know what else needs updating.",
      "Pass an exact, case-sensitive name; narrow with entity_type= (e.g. \"method\") when the name exists as more than one kind or in more than one place — sem_callers refuses with a candidate list rather than guessing.",
      "Zero callers is a real, useful answer: it means the entity is safe to remove or rename without updating anything else (per sem's syntax-level graph, not a real type-checker).",
    ],
    parameters: SemCallersParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemCallers(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
