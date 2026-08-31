import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

/**
 * The payload cost of ONE active tool definition as the model sees it:
 * the JSON size of {name, description, parameters}, in chars, with tokens
 * estimated as round(chars / 4).
 */
export interface ToolPayloadInfo {
  name: string;
  chars: number;
  tokens: number;
}

/** Aggregate payload size across every active tool for a turn. */
export interface ToolPayloadSummary {
  toolCount: number;
  totalChars: number;
  totalTokens: number;
  perTool: ToolPayloadInfo[];
}

/**
 * Just the fields the payload measurement needs -- a structural subset of
 * pi's ToolInfo (which additionally carries promptGuidelines/sourceInfo),
 * so ExtensionAPI.getAllTools() results are accepted without a cast.
 */
export interface MinimalToolInfo {
  name: string;
  description?: string;
  parameters?: unknown;
}

/**
 * Measures the tool-definition overhead the model actually pays per turn:
 * for each ACTIVE tool name (in order), the JSON size of its
 * {name, description, parameters} triple. Pure, so this is provable
 * without spawning any process.
 *
 * An active name with no matching definition is skipped (never a crash),
 * and a matched definition missing description/parameters is measured as
 * if they were empty. Totals are sums of the already-rounded per-tool
 * values; no re-rounding of a summed total.
 */
export function computeToolPayloadSize(
  activeToolNames: string[],
  allTools: MinimalToolInfo[],
): ToolPayloadSummary {
  // First definition wins for duplicate names, matching a plain
  // Array.prototype.find lookup over allTools.
  const byName = new Map<string, MinimalToolInfo>();
  for (const tool of allTools) {
    if (!byName.has(tool.name)) byName.set(tool.name, tool);
  }

  const perTool: ToolPayloadInfo[] = [];
  let totalChars = 0;
  let totalTokens = 0;

  for (const activeName of activeToolNames) {
    const tool = byName.get(activeName);
    if (!tool) continue;
    const payload = JSON.stringify({
      name: tool.name,
      description: tool.description ?? "",
      parameters: tool.parameters ?? {},
    });
    const chars = payload.length;
    const tokens = Math.round(chars / 4);
    perTool.push({ name: tool.name, chars, tokens });
    totalChars += chars;
    totalTokens += tokens;
  }

  return { toolCount: perTool.length, totalChars, totalTokens, perTool };
}

/**
 * Convenience wrapper for live sessions: measures whatever tools are
 * actually registered and active on the given extension API right now.
 */
export function getToolPayloadSummary(pi: ExtensionAPI): ToolPayloadSummary {
  return computeToolPayloadSize(pi.getActiveTools(), pi.getAllTools());
}
