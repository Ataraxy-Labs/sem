import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { DEFAULT_CONFIG } from "./default.ts";
import type { PiSemConfig } from "./types.ts";

/**
 * Resolve the active config. Reads `PI_SEM_CONFIG` (a path to a .json,
 * .ts, .js, or .mjs module) when set; falls back to `DEFAULT_CONFIG`.
 *
 * A .ts/.js module must export the config as its default export, or as a
 * named `config` export.
 */
export async function loadConfig(env: NodeJS.ProcessEnv = process.env): Promise<PiSemConfig> {
  const overridePath = env.PI_SEM_CONFIG;
  if (!overridePath) return DEFAULT_CONFIG;

  if (overridePath.endsWith(".json")) {
    const raw = await readFile(overridePath, "utf8");
    return JSON.parse(raw) as PiSemConfig;
  }

  const mod: Record<string, unknown> = await import(pathToFileURL(overridePath).href);
  const config = (mod.default ?? mod.config) as PiSemConfig | undefined;
  if (!config) {
    throw new Error(
      `PI_SEM_CONFIG=${overridePath} did not export a default or named "config" export.`,
    );
  }
  return config;
}
