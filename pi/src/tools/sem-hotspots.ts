import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { fetchHotspots } from "./internal/graph.ts";

/**
 * Which entities change most often — the exact same fetchHotspots wrapper
 * over `sem log --json` code mode's `sem.hotspots()` uses
 * (internal/graph.ts). Cheap: same one `sem log --json` call sem_cochange
 * also reads.
 */

export interface SemHotspotsParams {
  limit?: number;
}

export interface SemHotspotsDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemHotspotsOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

const SemHotspotsParamsSchema = Type.Object({
  limit: Type.Optional(Type.Integer({ minimum: 1, description: "Max hotspot rows to show (default 20)." })),
});

/** The tool's full orchestration, independent of pi's tool-registration glue so it can be driven directly in tests. Zero hotspots is a success (a plain sentence); only a genuine execution error comes back isError=true. */
export async function performSemHotspots(params: SemHotspotsParams, deps: SemHotspotsDeps): Promise<SemHotspotsOutcome> {
  const limit = params.limit ?? 20;

  let rows: Awaited<ReturnType<typeof fetchHotspots>>;
  try {
    rows = await fetchHotspots(deps.semBin, deps.cwd, limit, deps.signal);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { isError: true, text: `sem_hotspots: ${message}`, details: { limit, error: message } };
  }

  if (rows.length === 0) {
    return { isError: false, text: "sem_hotspots: no change history found (sem log's commit scan came back empty).", details: { limit, total: 0, hotspots: [] } };
  }

  const lines = rows.map((r) => `${r.entity} (${r.file}) — ${r.commits} commit${r.commits === 1 ? "" : "s"}`);

  return { isError: false, text: lines.join("\n"), details: { limit, total: rows.length, hotspots: rows } };
}

export interface RegisterSemHotspotsOptions {
  /** `sem` binary to shell out to for the log scan. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_hotspots` tool. Call once per pi extension load. */
export function registerSemHotspots(pi: ExtensionAPI, opts: RegisterSemHotspotsOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_hotspots",
    label: "Sem Hotspots",
    description: "Entities that change most often, from recent commit history — a cheap signal for where risk/churn concentrates.",
    promptSnippet: "List which entities change most often across recent commits",
    promptGuidelines: ["Use sem_hotspots to find where change churn concentrates before reviewing a repo's risk areas — not for finding a specific entity, just the ranking."],
    parameters: SemHotspotsParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemHotspots(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
