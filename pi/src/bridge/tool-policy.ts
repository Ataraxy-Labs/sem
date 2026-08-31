import type { RegisterServerResult } from "./register.ts";
import type { SessionPolicy } from "../config/types.ts";

/**
 * The tool lockdown's fail-closed core: the active tool set is ALWAYS
 * exactly `activeBuiltins + every tool that actually registered + extras`
 * (native tools registered independent of any MCP server, e.g. weave_edit)
 * -- pure, so this invariant is provable without spawning any process. A
 * server that failed contributes an empty `registeredToolNames`, which
 * this function handles the same as any other: nothing special to skip.
 */
export function computeActiveTools(
  sessionPolicy: SessionPolicy,
  results: RegisterServerResult[],
  extras: string[] = [],
): string[] {
  const registeredFromServers = results.flatMap((result) => result.registeredToolNames);
  return [...sessionPolicy.activeBuiltins, ...registeredFromServers, ...extras];
}
