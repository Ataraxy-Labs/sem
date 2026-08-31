import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { computeGraph, type GraphEdgeSummary, type GraphEntitySummary, type Ref } from "./internal/graph.ts";

/**
 * Dependency-graph neighborhood around one or more seed entities — a
 * compact text view over the exact same computeGraph BFS that code mode's
 * `sem.graph()` uses (internal/graph.ts), so tools mode gets the same
 * blast-radius/dependency-chain answer without composing sem_impact +
 * sem_callers by hand (the eval finding that motivated this tool: code
 * mode's graph task won on all arms using sem.graph()/sem.path() directly,
 * while tools mode had no native equivalent).
 */

export interface SemGraphParams {
  /** Required unless `seeds` is given. */
  seed?: string;
  /** Disambiguates `seed` when its name exists in more than one file. Ignored for `seeds`. */
  file?: string;
  /** Look up several seeds' combined neighborhood in one call instead of one call per seed. Takes priority over `seed` when non-empty. */
  seeds?: string[];
  hops?: number;
  direction?: "out" | "in" | "both";
  include_tests?: boolean;
}

export interface SemGraphDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemGraphOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

const SemGraphParamsSchema = Type.Object({
  seed: Type.Optional(Type.String({ description: "EXACT entity name to center the neighborhood on. Required unless seeds= is used." })),
  file: Type.Optional(Type.String({ description: "Disambiguates seed= when its name exists in more than one file. Ignored when seeds= is used." })),
  seeds: Type.Optional(
    Type.Array(Type.String(), { description: "Several seed entities in one call, sharing one combined neighborhood. Takes priority over seed= when given." }),
  ),
  hops: Type.Optional(Type.Integer({ minimum: 1, description: "How many edges out from the seed(s) to traverse. Default 2." })),
  direction: Type.Optional(
    Type.Union([Type.Literal("out"), Type.Literal("in"), Type.Literal("both")], {
      description: "'out': what the seed depends on. 'in': what depends on the seed (blast radius). 'both' (default): either direction.",
    }),
  ),
  include_tests: Type.Optional(Type.Boolean({ description: "Include test entities in the neighborhood. Default false — tests are usually noise for a dependency question." })),
});

/** Hard cap on rendered edge lines — the header and hop-group headers don't count. */
const MAX_RENDERED_EDGES = 150;

/**
 * Node-name BFS distance from the seed set, over the already-neighborhood-
 * scoped edges computeGraph returned (undirected, for display grouping
 * only — direction filtering already happened inside computeGraph's own
 * BFS). Used purely to group rendered edges by "hop N:" headers; not a
 * claim that this reproduces computeGraph's internal discovery order
 * exactly, just a deterministic, useful grouping over its output.
 */
function hopDistances(seedNames: string[], edges: GraphEdgeSummary[]): Map<string, number> {
  const adj = new Map<string, string[]>();
  const link = (a: string, b: string): void => {
    const bucket = adj.get(a);
    if (bucket) bucket.push(b);
    else adj.set(a, [b]);
  };
  for (const e of edges) {
    link(e.from, e.to);
    link(e.to, e.from);
  }

  const hop = new Map<string, number>();
  let frontier: string[] = [];
  for (const name of seedNames) {
    hop.set(name, 0);
    frontier.push(name);
  }
  let level = 0;
  while (frontier.length > 0) {
    const next: string[] = [];
    for (const id of frontier) {
      for (const neighbor of adj.get(id) ?? []) {
        if (!hop.has(neighbor)) {
          hop.set(neighbor, level + 1);
          next.push(neighbor);
        }
      }
    }
    frontier = next;
    level++;
  }
  return hop;
}

function renderEdgeLine(e: GraphEdgeSummary, fileOf: Map<string, string>): string {
  const fromFile = fileOf.get(e.from) ?? "?";
  const toFile = fileOf.get(e.to) ?? "?";
  return `  ${e.from} (${fromFile}) -> ${e.to} (${toFile})`;
}

function renderBody(seedNames: string[], nodes: GraphEntitySummary[], edges: GraphEdgeSummary[]): { lines: string[]; omitted: number } {
  const fileOf = new Map(nodes.map((n) => [n.name, n.file]));
  const hop = hopDistances(seedNames, edges);
  const byHop = new Map<number, GraphEdgeSummary[]>();
  for (const e of edges) {
    const level = Math.max(hop.get(e.from) ?? 0, hop.get(e.to) ?? 0);
    const bucket = byHop.get(level);
    if (bucket) bucket.push(e);
    else byHop.set(level, [e]);
  }

  const orderedLevels = [...byHop.keys()].sort((a, b) => a - b);
  const rendered: string[] = [];
  let shown = 0;
  let omitted = 0;

  for (const level of orderedLevels) {
    const group = byHop.get(level)!;
    if (shown >= MAX_RENDERED_EDGES) {
      omitted += group.length;
      continue;
    }
    const fit = group.slice(0, MAX_RENDERED_EDGES - shown);
    omitted += group.length - fit.length;
    rendered.push(`hop ${level === 0 ? "0 (seed)" : level}:`);
    for (const e of fit) rendered.push(renderEdgeLine(e, fileOf));
    shown += fit.length;
  }

  return { lines: rendered, omitted };
}

/**
 * The tool's full orchestration, independent of pi's tool-registration glue
 * so it can be driven directly in tests. Not-found and ambiguous seeds
 * (computeGraph -> resolveGraphNode) come back as refusals (isError=true);
 * a genuine empty neighborhood (a real, isolated entity) is a success.
 */
export async function performSemGraph(params: SemGraphParams, deps: SemGraphDeps): Promise<SemGraphOutcome> {
  const seeds: Ref[] =
    params.seeds !== undefined && params.seeds.length > 0
      ? params.seeds
      : params.seed !== undefined
        ? [params.file !== undefined ? { name: params.seed, file: params.file } : params.seed]
        : [];

  if (seeds.length === 0) {
    return { isError: true, text: "sem_graph: pass either seed= or seeds=[...].", details: { error: "missing seed" } };
  }

  const hops = params.hops ?? 2;
  const direction = params.direction ?? "both";
  const includeTests = params.include_tests ?? false;
  const seedLabel = seeds.map((s) => (typeof s === "string" ? s : s.name)).join(", ");

  let result: Awaited<ReturnType<typeof computeGraph>>;
  try {
    result = await computeGraph(deps.semBin, deps.cwd, params.seeds !== undefined && params.seeds.length > 0 ? seeds : seeds[0]!, { hops, direction, include_tests: includeTests }, deps.signal);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_graph: ${message}`,
      details: { seed: seedLabel, hops, direction, error: message },
    };
  }

  const seedNames = seeds.map((s) => (typeof s === "string" ? s : s.name));
  const { lines, omitted } = renderBody(seedNames, result.nodes, result.edges);
  if (omitted > 0) lines.push(`…${omitted} more edges (narrow with hops=/direction= or a more specific seed)`);

  const truncatedNote = result.truncated ? ", truncated at the node cap" : "";
  const header = `sem_graph: ${seedLabel} [hops=${hops}, direction=${direction}] — ${result.nodes.length} nodes, ${result.edges.length} edges${truncatedNote}`;

  return {
    isError: false,
    text: [header, ...lines].join("\n"),
    details: {
      seed: seedLabel,
      hops,
      direction,
      include_tests: includeTests,
      node_count: result.nodes.length,
      edge_count: result.edges.length,
      truncated: result.truncated,
      nodes: result.nodes,
      edges: result.edges,
    },
  };
}

export interface RegisterSemGraphOptions {
  /** `sem` binary to shell out to for the graph dump. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_graph` tool. Call once per pi extension load. */
export function registerSemGraph(pi: ExtensionAPI, opts: RegisterSemGraphOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_graph",
    label: "Sem Graph",
    description: "Dependency-graph neighborhood around one or more entities — blast radius (direction=in) or dependencies (direction=out), N hops.",
    promptSnippet: "Show the dependency-graph neighborhood (callers/callees, N hops) around one or more entities",
    promptGuidelines: [
      "Use sem_graph BEFORE composing sem_impact + sem_callers by hand for a blast-radius or dependency-chain question — it answers 'what's connected to X within N hops' in one call.",
      "direction=\"in\" answers blast radius (what depends on this); direction=\"out\" answers what this depends on; \"both\" (default) is either.",
      "Pass seeds=[...] to combine several entities' neighborhoods in one call instead of one sem_graph call per seed.",
    ],
    parameters: SemGraphParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemGraph(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
