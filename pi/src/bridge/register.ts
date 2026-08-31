import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { McpClient, type McpToolInfo } from "./mcp-client.ts";
import type { ServerConfig, ToolOverride } from "../config/types.ts";
import { applyTransform } from "./transforms.ts";

/**
 * Registers every allowlisted tool from one MCP server as a pi tool.
 *
 * The MCP `inputSchema` (JSON Schema) is wrapped as-is with `Type.Unsafe`,
 * matching pi's own idiom for non-TypeBox-native schemas (see `StringEnum`
 * in `@earendil-works/pi-ai`'s typebox-helpers). Pi passes `.parameters`
 * straight through to the provider payload and to the LLM without
 * re-validating with a TypeBox compiler, so the MCP server's own JSON
 * Schema — draft 2020-12, the subset MCP servers emit (type, properties,
 * required, enum, items, description, additionalProperties) — is accepted
 * unmodified. Proven against a real tools/call in the e2e smoke test.
 */

const PATH_LIKE_PARAM_NAMES = new Set(["path", "file", "file_path"]);

function normalizeLeadingAt(value: string): string {
  return value.startsWith("@") ? value.slice(1) : value;
}

/** Strip a leading "@" from path/file/file_path args some models add (pi docs note). */
function normalizePathArgs(args: unknown): unknown {
  if (typeof args !== "object" || args === null) return args;
  const record = args as Record<string, unknown>;
  let changed = false;
  const next: Record<string, unknown> = { ...record };
  for (const key of Object.keys(record)) {
    if (!PATH_LIKE_PARAM_NAMES.has(key)) continue;
    const value = record[key];
    if (typeof value === "string" && value.startsWith("@")) {
      next[key] = normalizeLeadingAt(value);
      changed = true;
    }
  }
  return changed ? next : args;
}

export interface RegisterServerResult {
  serverId: string;
  registeredToolNames: string[];
  /** Set when the server failed to start; the server contributes zero tools. */
  startError?: Error;
}

/** Starts one MCP server, lists its tools, and registers the allowlisted subset with pi. */
export async function registerServerTools(
  pi: ExtensionAPI,
  config: ServerConfig,
  onStderr?: (serverId: string, chunk: string) => void,
): Promise<{ client: McpClient; result: RegisterServerResult }> {
  const client = new McpClient({
    id: config.id,
    command: config.command,
    args: config.args,
    env: config.env,
    cwd: config.cwd,
    requestTimeoutMs: config.requestTimeoutMs,
    onStderr: onStderr ? (chunk) => onStderr(config.id, chunk) : undefined,
  });

  let tools: McpToolInfo[];
  try {
    await client.start();
    tools = await client.listTools();
  } catch (err) {
    // Widened to cover both start() and listTools(): either can reject
    // (the server can complete `initialize` and still die/error before
    // answering `tools/list`), and this function's documented contract is
    // "server-adjacent failures are captured in the result, never thrown."
    // stop() the client here -- once we return, nothing else can reach it,
    // and McpClient's onExit() would otherwise fire its "one allowed
    // restart" on the abandoned child with no owner left to stop it.
    await client.stop();
    return {
      client,
      result: {
        serverId: config.id,
        registeredToolNames: [],
        startError: err instanceof Error ? err : new Error(String(err)),
      },
    };
  }

  const registeredToolNames: string[] = [];

  for (const tool of tools) {
    const override = config.tools[tool.name];
    if (!override) continue; // Not allowlisted.
    const registeredName = registerOneTool(pi, client, tool, override === true ? {} : override);
    registeredToolNames.push(registeredName);
  }

  return { client, result: { serverId: config.id, registeredToolNames } };
}

function registerOneTool(
  pi: ExtensionAPI,
  client: McpClient,
  tool: McpToolInfo,
  override: ToolOverride,
): string {
  const name = override.name ?? tool.name;
  const description = override.description ?? tool.description ?? `MCP tool ${tool.name} from ${client.id}`;
  const parameters = Type.Unsafe(tool.inputSchema);

  pi.registerTool({
    name,
    label: name,
    description,
    promptSnippet: override.promptSnippet,
    promptGuidelines: override.promptGuidelines,
    parameters,
    async execute(_toolCallId, params, signal) {
      const normalized = normalizePathArgs(params);
      const result = await client.callTool(tool.name, normalized, signal);
      const rawText = result.content.map((c) => c.text).join("\n");
      // Never transform an error result -- outline etc. expect the tool's
      // normal success shape, and error text should reach the LLM verbatim.
      const text = result.isError
        ? rawText
        : applyTransform(override.transform, rawText, { toolName: tool.name, args: normalized });
      return {
        content: [{ type: "text", text: text || "(empty result)" }],
        // `raw` keeps the untransformed text (e.g. the tool's raw JSON)
        // available to renderers even when `content` has been reshaped.
        details: { server: client.id, tool: tool.name, isError: result.isError ?? false, raw: rawText },
      };
    },
  });

  return name;
}
