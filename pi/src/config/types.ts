/**
 * Config shape for the pi-sem bridge: which MCP servers to spawn, which of
 * their tools to expose to pi, and what built-in pi tools stay active.
 */

/** Per-tool override. `true` means "expose with the MCP server's own name/description". */
export interface ToolOverride {
  /** Rename the tool as it appears to pi/the LLM. Defaults to the MCP tool name. */
  name?: string;
  /** Replace the MCP tool's description. Defaults to the MCP tool's own description. */
  description?: string;
  /** One-line entry for the system prompt's "Available tools" section. */
  promptSnippet?: string;
  /** Guideline bullets appended to the system prompt's "Guidelines" section. */
  promptGuidelines?: string[];
  /**
   * Id of a pure result transform (see src/bridge/transforms.ts) to run over
   * the MCP tool's raw text result before it reaches the LLM. Unknown ids
   * are ignored (raw text passes through unchanged) rather than erroring,
   * so a typo here degrades gracefully instead of breaking the tool.
   */
  transform?: string;
}

export interface ServerConfig {
  /** Stable id for this server, used in status output and log lines. */
  id: string;
  /** Executable to spawn (resolved via PATH unless absolute). */
  command: string;
  args?: string[];
  env?: Record<string, string>;
  cwd?: string;
  /** Per-call timeout in milliseconds. Defaults to 30_000. */
  requestTimeoutMs?: number;
  /**
   * Allowlist of MCP tool names to expose. Only tools present as keys here
   * are registered with pi; everything else the server offers is ignored.
   */
  tools: Record<string, ToolOverride | true>;
}

export interface SessionPolicy {
  /** Built-in pi tools that stay active alongside the registered MCP tools. */
  activeBuiltins: string[];
}

export interface PiSemConfig {
  servers: ServerConfig[];
  sessionPolicy: SessionPolicy;
  /**
   * Extra guidance appended to pi's system prompt while this extension is
   * active (entity-first workflow steering, tool sequencing, etc.).
   */
  systemPromptAddendum?: string;
}
