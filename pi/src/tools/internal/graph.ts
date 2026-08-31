import { runCommand } from "./proc.ts";

/**
 * Repo-wide dependency-graph and history primitives, backed by `sem graph
 * --json` and `sem log --json`.
 *
 * Extracted from src/codemode/api.ts (which introduced these first, for
 * code-mode's `sem.graph()`/`sem.path()`/`sem.cochange()`/`sem.hotspots()`)
 * so sem_graph/sem_path/sem_cochange/sem_hotspots (native tools mode) reuse
 * the exact same BFS/JSON-shape plumbing instead of a second copy drifting
 * from it. api.ts's `sem.*` wrappers now call the functions here directly;
 * this file owns zero sandbox/deps-shape concerns of its own — every
 * function takes `semBin`/`cwd` plainly, the same convention
 * internal/entities.ts's extractEntities already uses.
 *
 * Host-algebra finding that motivated graph()/path() existing at all
 * (verified empirically, not assumed): `sem impact --depth N` is
 * ASYMMETRIC — `dependencies` (the "out" direction) is ALWAYS exactly 1 hop
 * regardless of --depth; only `dependents`/`impact.entities[]` (the "in"
 * direction) is genuinely depth-aware. `sem impact` therefore cannot serve
 * a uniform out/in/both hop-neighborhood query at all — `sem graph --json`
 * (the full entity+edge dump) is the only primitive that can, so
 * computeGraph/computePath fetch it once and do local BFS over its edges in
 * Node, uniformly, in whichever direction was asked (~12-220ms warm,
 * measured against repos from ~4400 to ~2200 entities — see codemode's
 * DESIGN.md for the full timing writeup).
 */

export interface RawGraphEntity {
  id: string;
  name: string;
  entityType: string;
  filePath: string;
  parentId: string | null;
  startLine: number;
  endLine: number;
}

export interface RawGraphEdge {
  fromEntity: string;
  toEntity: string;
  refType: string;
}

export interface RawGraph {
  entities: RawGraphEntity[];
  edges: RawGraphEdge[];
}

export interface GraphEntitySummary {
  name: string;
  type: string;
  file: string;
  parent_name: string | null;
  start_line: number;
  end_line: number;
}

export type Ref = string | { name: string; file?: string };

export async function fetchGraph(semBin: string, cwd: string, signal?: AbortSignal): Promise<RawGraph> {
  const result = await runCommand(semBin, ["graph", "--json", "--no-default-excludes"], cwd, signal);
  /*
   * `exitCode` is checked BEFORE parsing, matching sem-find.ts/sem-callers.ts/
   * sem-grep.ts's convention: a real sem failure (bad flag, corrupted index, a
   * crash) writes its actual, actionable message to stderr with empty stdout;
   * unconditionally JSON.parse-ing stdout converted that into a generic
   * "invalid JSON" message that named neither the cause nor a next step. This
   * exact shape was misdiagnosed as environmental noise for three sessions.
   */
  if (result.exitCode !== 0) {
    throw new Error(`"${semBin} graph --json" failed (exit ${result.exitCode}): ${result.stderr.trim() || "no stderr output"}`);
  }
  try {
    return JSON.parse(result.stdout) as RawGraph;
  } catch (err) {
    throw new Error(`"${semBin} graph --json" produced invalid JSON: ${err instanceof Error ? err.message : String(err)}`);
  }
}

export function parentNameFromGraphId(parentId: string | null): string | null {
  if (!parentId) return null;
  const parts = parentId.split("::");
  const last = parts[parts.length - 1];
  return last && last.length > 0 ? last : null;
}

export function graphEntityToSummary(e: RawGraphEntity): GraphEntitySummary {
  return { name: e.name, type: e.entityType, file: e.filePath, parent_name: parentNameFromGraphId(e.parentId), start_line: e.startLine, end_line: e.endLine };
}

export function refName(ref: Ref): string {
  return typeof ref === "string" ? ref : ref.name;
}

export function findGraphNodes(g: RawGraph, ref: Ref): RawGraphEntity[] {
  const file = typeof ref === "string" ? undefined : ref.file;
  const name = refName(ref);
  return g.entities.filter((e) => e.name === name && (file === undefined || e.filePath === file || e.filePath === file.replace(/^@/, "")));
}

/** Same disambiguation spirit as weave_edit/sem_read's entity resolution — never guesses among several matches, throws with the candidate files instead. */
export function resolveGraphNode(g: RawGraph, ref: Ref): RawGraphEntity {
  const matches = findGraphNodes(g, ref);
  const name = refName(ref);
  if (matches.length === 0) throw new Error(`no entity named "${name}" found in the dependency graph.`);
  if (matches.length > 1) {
    const files = [...new Set(matches.map((m) => m.filePath))];
    throw new Error(`"${name}" is ambiguous — found in ${files.length} place(s): ${files.join(", ")}. Pass { name, file } to disambiguate.`);
  }
  return matches[0]!;
}

export const MAX_GRAPH_NODES = 300;

export interface GraphOpts {
  hops?: number;
  direction?: "out" | "in" | "both";
  include_tests?: boolean;
}

/** BFS over the full graph's edges, capped at MAX_GRAPH_NODES, direction-aware, test entities excluded unless include_tests. */
export function bfsNeighborhood(
  g: RawGraph,
  seedIds: Set<string>,
  hops: number,
  direction: "out" | "in" | "both",
  includeTests: boolean,
): { nodeIds: Set<string>; edges: RawGraphEdge[]; cappedMidExploration: boolean } {
  const byId = new Map(g.entities.map((e) => [e.id, e]));
  const outEdges = new Map<string, RawGraphEdge[]>();
  const inEdges = new Map<string, RawGraphEdge[]>();
  function pushInto<K>(m: Map<K, RawGraphEdge[]>, key: K, edge: RawGraphEdge): void {
    const bucket = m.get(key);
    if (bucket) bucket.push(edge);
    else m.set(key, [edge]);
  }
  for (const e of g.edges) {
    pushInto(outEdges, e.fromEntity, e);
    pushInto(inEdges, e.toEntity, e);
  }

  const isTest = (id: string) => byId.get(id)?.entityType === "test";
  const visited = new Set(seedIds);
  const usedEdges: RawGraphEdge[] = [];
  let frontier = [...seedIds];
  let cappedMidExploration = false;

  for (let hop = 0; hop < hops && visited.size < MAX_GRAPH_NODES && frontier.length > 0; hop++) {
    const next: string[] = [];
    for (const id of frontier) {
      const candidates = [...(direction !== "in" ? (outEdges.get(id) ?? []) : []), ...(direction !== "out" ? (inEdges.get(id) ?? []) : [])];
      for (const e of candidates) {
        const neighborId = e.fromEntity === id ? e.toEntity : e.fromEntity;
        if (!includeTests && isTest(neighborId)) continue;
        usedEdges.push(e);
        if (!visited.has(neighborId)) {
          if (visited.size < MAX_GRAPH_NODES) {
            visited.add(neighborId);
            next.push(neighborId);
          } else {
            cappedMidExploration = true;
          }
        }
      }
    }
    frontier = next;
  }

  return { nodeIds: visited, edges: usedEdges, cappedMidExploration };
}

export interface GraphEdgeSummary {
  from: string;
  to: string;
  kind: string;
}

export interface GraphResult {
  nodes: GraphEntitySummary[];
  edges: GraphEdgeSummary[];
  truncated: boolean;
}

export async function computeGraph(semBin: string, cwd: string, seed: Ref | Ref[], opts: GraphOpts, signal?: AbortSignal): Promise<GraphResult> {
  const g = await fetchGraph(semBin, cwd, signal);
  const seeds = (Array.isArray(seed) ? seed : [seed]).map((r) => resolveGraphNode(g, r));
  const seedIds = new Set(seeds.map((s) => s.id));
  const { nodeIds, edges, cappedMidExploration } = bfsNeighborhood(g, seedIds, opts.hops ?? 2, opts.direction ?? "both", opts.include_tests ?? false);

  const byId = new Map(g.entities.map((e) => [e.id, e]));
  const nodes = [...nodeIds].map((id) => graphEntityToSummary(byId.get(id)!));

  const seenEdgeKeys = new Set<string>();
  const edgeList = edges
    .filter((e) => nodeIds.has(e.fromEntity) && nodeIds.has(e.toEntity))
    .filter((e) => {
      const key = `${e.fromEntity}->${e.toEntity}:${e.refType}`;
      if (seenEdgeKeys.has(key)) return false;
      seenEdgeKeys.add(key);
      return true;
    })
    .map((e) => ({ from: byId.get(e.fromEntity)!.name, to: byId.get(e.toEntity)!.name, kind: e.refType }));

  return { nodes, edges: edgeList, truncated: cappedMidExploration };
}

export interface PathOpts {
  max_hops?: number;
  /**
   * "out" (default): a DIRECTED call/reference chain from a to b — what
   * "how does a reach b" actually means, and the only direction guaranteed
   * to return a real sequence of calls. "in" walks edges backward. "any" is
   * the old undirected behavior, kept as an explicit opt-in only: a node
   * that merely calls BOTH a and b creates a shorter but SPURIOUS
   * "connection" through itself that is not a call chain at all — the
   * oracle-floor work caught this returning a wrong choke-point answer on a
   * real fixture, so undirected must never be the silent default.
   */
  direction?: "out" | "in" | "any";
}

/** BFS over the graph in the requested direction — directed call-flow by default; see PathOpts.direction for why undirected is opt-in only. */
export async function computePath(semBin: string, cwd: string, a: Ref, b: Ref, opts: PathOpts, signal?: AbortSignal): Promise<GraphEntitySummary[] | null> {
  const g = await fetchGraph(semBin, cwd, signal);
  const nodeA = resolveGraphNode(g, a);
  const nodeB = resolveGraphNode(g, b);
  const maxHops = opts.max_hops ?? 6;
  const direction = opts.direction ?? "out";

  const adj = new Map<string, string[]>();
  function link(from: string, to: string): void {
    const bucket = adj.get(from);
    if (bucket) bucket.push(to);
    else adj.set(from, [to]);
  }
  for (const e of g.edges) {
    if (direction === "out" || direction === "any") link(e.fromEntity, e.toEntity);
    if (direction === "in" || direction === "any") link(e.toEntity, e.fromEntity);
  }

  if (nodeA.id === nodeB.id) return [graphEntityToSummary(nodeA)];

  const visited = new Set([nodeA.id]);
  const parent = new Map<string, string>();
  let frontier = [nodeA.id];
  let found = false;

  for (let hop = 0; hop < maxHops && !found && frontier.length > 0; hop++) {
    const next: string[] = [];
    for (const id of frontier) {
      for (const neighbor of adj.get(id) ?? []) {
        if (visited.has(neighbor)) continue;
        visited.add(neighbor);
        parent.set(neighbor, id);
        if (neighbor === nodeB.id) {
          found = true;
          break;
        }
        next.push(neighbor);
      }
      if (found) break;
    }
    frontier = next;
  }

  if (!found) return null;

  const chainIds: string[] = [nodeB.id];
  let cursor = nodeB.id;
  while (cursor !== nodeA.id) {
    const p = parent.get(cursor)!;
    chainIds.unshift(p);
    cursor = p;
  }

  const byId = new Map(g.entities.map((e) => [e.id, e]));
  return chainIds.map((id) => graphEntityToSummary(byId.get(id)!));
}

export interface HotspotRow {
  entity: string;
  file: string;
  commits: number;
}

/** commits-scanned is fixed at sem's own CLI default (50) — `limit` here caps how many hotspot ROWS come back, a client-side slice, same "CLI has no result-limit flag" convention as sem_find/sem_grep elsewhere in this package. */
export async function fetchHotspots(semBin: string, cwd: string, limit: number | undefined, signal?: AbortSignal): Promise<HotspotRow[]> {
  const result = await runCommand(semBin, ["log", "--json"], cwd, signal);
  // Same before-parse exitCode check as fetchGraph — see its comment.
  if (result.exitCode !== 0) {
    throw new Error(`"${semBin} log --json" failed (exit ${result.exitCode}): ${result.stderr.trim() || "no stderr output"}`);
  }
  let parsed: { hotspots?: HotspotRow[] };
  try {
    parsed = JSON.parse(result.stdout) as { hotspots?: HotspotRow[] };
  } catch (err) {
    throw new Error(`invalid JSON from "${semBin} log --json": ${err instanceof Error ? err.message : String(err)}`);
  }
  return (parsed.hotspots ?? []).slice(0, limit ?? 20);
}

export interface CoChangeSide {
  entity: string;
  file: string;
  commits: number;
}

export interface CoChangePair {
  a: CoChangeSide;
  b: CoChangeSide;
  together: number;
  confidence: number;
}

export async function fetchCoChange(semBin: string, cwd: string, entityName: string, limit: number | undefined, signal?: AbortSignal): Promise<CoChangePair[]> {
  const result = await runCommand(semBin, ["log", "--json"], cwd, signal);
  // Same before-parse exitCode check as fetchGraph — see its comment.
  if (result.exitCode !== 0) {
    throw new Error(`"${semBin} log --json" failed (exit ${result.exitCode}): ${result.stderr.trim() || "no stderr output"}`);
  }
  let parsed: { coChanges?: CoChangePair[] };
  try {
    parsed = JSON.parse(result.stdout) as { coChanges?: CoChangePair[] };
  } catch (err) {
    throw new Error(`invalid JSON from "${semBin} log --json": ${err instanceof Error ? err.message : String(err)}`);
  }
  const pairs = (parsed.coChanges ?? []).filter((p) => p.a.entity === entityName || p.b.entity === entityName);
  return pairs.slice(0, limit ?? 20);
}
