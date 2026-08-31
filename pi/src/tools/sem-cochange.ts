import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { fetchCoChange } from "./internal/graph.ts";

/**
 * Which other entities tend to change alongside one named entity — the
 * exact same fetchCoChange wrapper over `sem log --json` code mode's
 * `sem.cochange()` uses (internal/graph.ts). A hidden-coupling signal sem's
 * own dependency graph can't see (two entities with no syntactic edge
 * between them that nonetheless keep changing together).
 */

export interface SemCochangeParams {
  entity: string;
  limit?: number;
}

export interface SemCochangeDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemCochangeOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

const SemCochangeParamsSchema = Type.Object({
  entity: Type.String({ description: "EXACT entity name to find co-changing pairs for." }),
  limit: Type.Optional(Type.Integer({ minimum: 1, description: "Max pairs to show (default 20)." })),
});

/** The tool's full orchestration, independent of pi's tool-registration glue so it can be driven directly in tests. Zero co-changing pairs is a real, useful success (a plain sentence); only a genuine execution error comes back isError=true. */
export async function performSemCochange(params: SemCochangeParams, deps: SemCochangeDeps): Promise<SemCochangeOutcome> {
  const limit = params.limit ?? 20;

  let pairs: Awaited<ReturnType<typeof fetchCoChange>>;
  try {
    pairs = await fetchCoChange(deps.semBin, deps.cwd, params.entity, limit, deps.signal);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { isError: true, text: `sem_cochange: ${message}`, details: { entity: params.entity, limit, error: message } };
  }

  if (pairs.length === 0) {
    return {
      isError: false,
      text: `sem_cochange: no entities found that consistently change alongside "${params.entity}" (per sem's commit-history scan).`,
      details: { entity: params.entity, limit, total: 0, pairs: [] },
    };
  }

  const lines = pairs.map((p) => {
    const other = p.a.entity === params.entity ? p.b : p.a;
    return `${params.entity} <-> ${other.entity} (${other.file}) — together ${p.together}x, confidence ${(p.confidence * 100).toFixed(0)}%`;
  });

  return { isError: false, text: lines.join("\n"), details: { entity: params.entity, limit, total: pairs.length, pairs } };
}

export interface RegisterSemCochangeOptions {
  /** `sem` binary to shell out to for the log scan. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_cochange` tool. Call once per pi extension load. */
export function registerSemCochange(pi: ExtensionAPI, opts: RegisterSemCochangeOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_cochange",
    label: "Sem Co-change",
    description: "Entities that tend to change alongside a named entity in commit history — surfaces hidden coupling sem's dependency graph can't see.",
    promptSnippet: "Find entities that historically change alongside a named entity, even with no direct code dependency",
    promptGuidelines: [
      "Use sem_cochange when a change might have hidden ripple effects sem_graph/sem_callers wouldn't catch — two files that always change together but share no syntactic edge.",
    ],
    parameters: SemCochangeParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemCochange(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
