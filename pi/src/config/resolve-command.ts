import { accessSync, constants } from "node:fs";
import { basename } from "node:path";
import type { PiSemConfig } from "./types.ts";

/**
 * Resolve each configured server command ONCE, before anything consumes it —
 * both the bridged-tools registration loop and the "weave" server lookup that
 * feeds weave_edit's / sem_code's live-coordination Coordinator read the same
 * resolved value, so the two paths can never disagree about which binary runs.
 *
 * Why this exists: allowlist.json historically pointed the "weave" server at
 * an absolute path to an UNTRACKED, machine-local cargo build artifact
 * (weave/target/release/weave-mcp). That file has vanished mid-session twice —
 * deleted by release prep once, and absent entirely on a fresh machine —
 * silently breaking every consumer downstream. A config path is a preference,
 * not a guarantee; resolution order:
 *
 *   1. `PI_SEM_<ID>_MCP_BIN` env var — explicit operator override, wins
 *      unconditionally (this also makes the variable work in the common case;
 *      previously weave-edit.ts only honored it when NO server was configured).
 *   2. The configured path, if it exists and is executable.
 *   3. The path's basename, handed to spawn() for ordinary $PATH resolution,
 *      with one warning naming the substitution.
 *
 * Bare names (no "/") pass through untouched — spawn() already does $PATH
 * resolution for those, and `sem` is configured that way on purpose.
 */
export function resolveCommand(
  serverId: string,
  command: string,
  warn: (message: string) => void,
): string {
  const envOverride = process.env[`PI_SEM_${serverId.toUpperCase()}_MCP_BIN`];
  if (envOverride) return envOverride;
  if (!command.includes("/")) return command;
  try {
    accessSync(command, constants.X_OK);
    return command;
  } catch {
    const fallback = basename(command);
    warn(
      `pi-sem: server "${serverId}" configured command is missing or not executable (${command}); falling back to "${fallback}" on $PATH.`,
    );
    return fallback;
  }
}

/** Shallow-copies the config with every server command resolved via {@link resolveCommand}. */
export function resolveConfigCommands(config: PiSemConfig, warn: (message: string) => void): PiSemConfig {
  return {
    ...config,
    servers: config.servers.map((s) => ({ ...s, command: resolveCommand(s.id, s.command, warn) })),
  };
}
