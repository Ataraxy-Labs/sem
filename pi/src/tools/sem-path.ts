import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { computePath, type Ref } from "./internal/graph.ts";

/**
 * The shortest connection between two entities in the dependency graph —
 * the exact same computePath BFS code mode's `sem.path()` uses
 * (internal/graph.ts), as one compact chain line instead of composing
 * sem_impact/sem_callers hops by hand to answer "how are A and B
 * connected."
 */

export interface SemPathParams {
  a: string;
  /** Disambiguates `a` when its name exists in more than one file. */
  a_file?: string;
  b: string;
  /** Disambiguates `b` when its name exists in more than one file. */
  b_file?: string;
  max_hops?: number;
  /** "out" (default): directed call chain a->b. "in": reverse. "any": the old undirected behavior, opt-in only. */
  direction?: "out" | "in" | "any";
}

export interface SemPathDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemPathOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

const SemPathParamsSchema = Type.Object({
  a: Type.String({ description: "EXACT name of the first entity." }),
  a_file: Type.Optional(Type.String({ description: "Disambiguates a= when its name exists in more than one file." })),
  b: Type.String({ description: "EXACT name of the second entity." }),
  b_file: Type.Optional(Type.String({ description: "Disambiguates b= when its name exists in more than one file." })),
  max_hops: Type.Optional(Type.Integer({ minimum: 1, description: "How far to search before giving up. Default 6." })),
  direction: Type.Optional(Type.Union([Type.Literal("out"), Type.Literal("in"), Type.Literal("any")], { description: "out (default): directed call chain a->b. in: reverse. any: undirected -- can connect via a shared caller that is NOT a real call chain; opt-in only." })),
});

/**
 * The tool's full orchestration, independent of pi's tool-registration glue
 * so it can be driven directly in tests. Not-found/ambiguous a=/b= come
 * back as refusals (isError=true); no connection within max_hops is a
 * success (a plain sentence, isError=false) — it's a real, useful answer,
 * not a failure.
 */
export async function performSemPath(params: SemPathParams, deps: SemPathDeps): Promise<SemPathOutcome> {
  const a: Ref = params.a_file !== undefined ? { name: params.a, file: params.a_file } : params.a;
  const b: Ref = params.b_file !== undefined ? { name: params.b, file: params.b_file } : params.b;
  const maxHops = params.max_hops ?? 6;

  let chain: Awaited<ReturnType<typeof computePath>>;
  try {
    chain = await computePath(deps.semBin, deps.cwd, a, b, { max_hops: maxHops, direction: params.direction }, deps.signal);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_path: ${message}`,
      details: { a: params.a, b: params.b, max_hops: maxHops, error: message },
    };
  }

  if (chain === null) {
    return {
      isError: false,
      text: `sem_path: no connection found between "${params.a}" and "${params.b}" within ${maxHops} hops.`,
      details: { a: params.a, b: params.b, max_hops: maxHops, found: false, chain: null },
    };
  }

  const line = chain.map((n) => `${n.name} (${n.file}:${n.start_line})`).join(" -> ");

  return {
    isError: false,
    text: `sem_path: ${line}`,
    details: { a: params.a, b: params.b, max_hops: maxHops, found: true, hops: chain.length - 1, chain },
  };
}

export interface RegisterSemPathOptions {
  /** `sem` binary to shell out to for the graph dump. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_path` tool. Call once per pi extension load. */
export function registerSemPath(pi: ExtensionAPI, opts: RegisterSemPathOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_path",
    label: "Sem Path",
    description: "Shortest directed call chain from a to b, e.g. A -> B -> C, or 'no path within N hops'. direction: out (default) | in | any (undirected, opt-in).",
    promptSnippet: "Find the shortest dependency-graph chain connecting two named entities",
    promptGuidelines: [
      "Use sem_path to answer 'how are these two things connected' directly instead of walking sem_callers/sem_impact by hand from both ends.",
      "A null result (no path found) is a real answer — it means the two entities are genuinely unconnected within max_hops, not that the lookup failed.",
    ],
    parameters: SemPathParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemPath(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
