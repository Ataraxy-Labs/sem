import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { fromAllowlistFile, isAllowlistFile } from "./allowlist.ts";
import type { PiSemConfig } from "./types.ts";

/**
 * Broad, uncurated starter set (bare `true` = expose with the MCP server's
 * own name/description). Used only if `allowlist.json` can't be read or
 * fails to parse as an `AllowlistFile` — keeps the extension usable even
 * if the curated file is missing or malformed, at the cost of losing the
 * renames/descriptions/promptGuidelines and systemPromptAddendum.
 */
const FALLBACK_CONFIG: PiSemConfig = {
  servers: [
    {
      id: "sem",
      command: "sem",
      args: ["mcp"],
      tools: {
        sem_context: true,
        sem_entities: true,
        sem_impact: true,
        sem_diff: true,
        sem_blame: true,
        sem_log: true,
      },
    },
    {
      id: "weave",
      // Bare name, resolved via $PATH -- override with PI_SEM_WEAVE_MCP_BIN
      // (see resolve-command.ts) if weave-mcp isn't on PATH.
      command: "weave-mcp",
      args: [],
      tools: {
        weave_extract_entities: true,
        weave_get_dependencies: true,
        weave_get_dependents: true,
        weave_impact_analysis: true,
        weave_status: true,
        weave_diff: true,
        weave_claim_entity: true,
        weave_release_entity: true,
        weave_update_entity_content: true,
        weave_get_entity_content: true,
      },
    },
  ],
  sessionPolicy: {
    activeBuiltins: ["bash", "write"],
  },
};

function loadDefaultConfig(): PiSemConfig {
  const allowlistPath = join(dirname(fileURLToPath(import.meta.url)), "allowlist.json");
  let parsed: unknown;
  try {
    parsed = JSON.parse(readFileSync(allowlistPath, "utf8"));
  } catch (err) {
    console.error(
      `pi-sem: could not read/parse ${allowlistPath} (${err instanceof Error ? err.message : String(err)}); using the uncurated fallback config.`,
    );
    return FALLBACK_CONFIG;
  }

  if (!isAllowlistFile(parsed)) {
    console.error(`pi-sem: ${allowlistPath} does not match the expected allowlist shape; using the uncurated fallback config.`);
    return FALLBACK_CONFIG;
  }

  return fromAllowlistFile(parsed);
}

/**
 * The default config: the curated allowlist from `allowlist.json`,
 * converted to the runtime `PiSemConfig` shape. Falls back to
 * `FALLBACK_CONFIG` if that file is missing or malformed.
 */
export const DEFAULT_CONFIG: PiSemConfig = loadDefaultConfig();

/**
 * Used only when `PI_SEM_CONFIG` is SET but fails to load/parse
 * (extensions/pi-sem.ts's `session_start` catch around `loadConfig()`) --
 * never for the absent-env-var case, which is a different code path
 * entirely (`loadConfig()` returns `DEFAULT_CONFIG` directly, no throw, no
 * warning: that's normal first-run behavior, not a failure).
 *
 * A config we couldn't read is a config whose intended allowlist we don't
 * know. Falling back to `DEFAULT_CONFIG` in that state -- as this package
 * did before this fix -- grants the full curated bridged-MCP-server
 * surface (sem_impact/sem_diff/weave_impact/etc, via real spawned
 * processes) even though the operator explicitly set a PI_SEM_CONFIG,
 * i.e. explicitly asked for something OTHER than the default. That's a
 * broader authority grant than anything the unreadable config ever
 * actually made -- the same "ambient authority" shape as the code-mode
 * fail-open (83ea54f): a grant with no traceable cause in what was
 * actually, successfully configured.
 *
 * `FAIL_CLOSED_CONFIG` withholds bridged MCP-server tools entirely
 * (`servers: []` -- no process is ever spawned for this fallback, unlike
 * the DEFAULT_CONFIG fallback it replaces) while keeping the SAME
 * `sessionPolicy`/`systemPromptAddendum` as `DEFAULT_CONFIG`: neither of
 * those is server authority (bash/write are always wrapped by pi-sem's own
 * audit regardless of config; the addendum is descriptive prompt text
 * naming only the native tools, which stay unconditional -- see below).
 * The native per-lookup tools (weave_edit, sem_outline, sem_read,
 * sem_find, sem_grep, sem_callers, sem_graph, sem_path, sem_hotspots,
 * sem_cochange) are registered unconditionally in
 * startServersAndRegisterTools's tools-mode branch -- they never depend on
 * `config.servers` -- so this fallback still leaves a fully usable
 * session, not a crippled one; it withholds exactly the one grant (bridged
 * MCP tools) the unreadable config never actually made.
 */
export const FAIL_CLOSED_CONFIG: PiSemConfig = {
  servers: [],
  sessionPolicy: DEFAULT_CONFIG.sessionPolicy,
  systemPromptAddendum: DEFAULT_CONFIG.systemPromptAddendum,
};
