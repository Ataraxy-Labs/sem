import type { PiSemConfig, ServerConfig, ToolOverride } from "./types.ts";

/**
 * On-disk shape of `src/config/allowlist.json`: the curated, LLM-facing
 * tool descriptions and renames. This is a *data* shape, distinct from the
 * runtime `PiSemConfig` the bridge (`register.ts`, `mcp-client.ts`) is
 * built against:
 *
 * - `servers` here is keyed by server id (`{ sem: {...}, weave: {...} }`);
 *   `PiSemConfig.servers` is an array of `{ id, ... }` — the array shape
 *   is what `McpClient`/`registerServerTools` are already wired to and
 *   tested against, so the conversion happens here rather than changing
 *   the runtime shape.
 * - every tool entry here is always a curated `{ name, description,
 *   promptGuidelines }` object (never the bare `true` shorthand
 *   `PiSemConfig` also allows for "expose with the MCP server's own
 *   name/description").
 * - `activeBuiltins` and `systemPromptAddendum` are top-level here;
 *   `PiSemConfig` nests the former under `sessionPolicy`.
 */
export interface AllowlistFile {
  servers: Record<
    string,
    {
      command: string;
      args?: string[];
      env?: Record<string, string>;
      cwd?: string;
      requestTimeoutMs?: number;
      tools: Record<string, ToolOverride>;
    }
  >;
  activeBuiltins: string[];
  systemPromptAddendum?: string;
}

export function isAllowlistFile(value: unknown): value is AllowlistFile {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return (
    typeof record.servers === "object" &&
    record.servers !== null &&
    Array.isArray(record.activeBuiltins)
  );
}

/** Converts the curated on-disk allowlist shape into the runtime `PiSemConfig`. */
export function fromAllowlistFile(file: AllowlistFile): PiSemConfig {
  const servers: ServerConfig[] = Object.entries(file.servers).map(([id, server]) => ({
    id,
    command: server.command,
    args: server.args,
    env: server.env,
    cwd: server.cwd,
    requestTimeoutMs: server.requestTimeoutMs,
    tools: server.tools,
  }));

  return {
    servers,
    sessionPolicy: { activeBuiltins: file.activeBuiltins },
    systemPromptAddendum: file.systemPromptAddendum,
  };
}
