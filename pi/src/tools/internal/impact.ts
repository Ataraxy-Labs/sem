import { runCommand } from "./proc.ts";

/**
 * Cheap, best-effort cross-file reference check via `sem impact --dependents`.
 * sem's graph is syntax-level def/use linking, not a real type-checker — it
 * does not model visibility (TS export, Rust pub, ...), so a same-named
 * replace can look identically "referenced" before and after even when a
 * visibility change broke the reference for real. This is informational,
 * never a correctness gate: report what sem's own graph can and can't see,
 * honestly, and never block on it.
 *
 * `--no-default-excludes` is deliberate: sem's own default excludes drop
 * any path containing "fixtures"/"vendor"/"generated"/"benchmarks" — common
 * real directory names that legitimately hold code other files depend on.
 * Silently under-reporting dependents because of that naming coincidence
 * would undermine the "report honestly" goal this check exists for.
 */

export interface DependentInfo {
  file: string;
  name: string;
  type: string;
}

export type DependentsResult = { ok: true; dependents: DependentInfo[] } | { ok: false; reason: string };

export async function checkDependents(
  semBin: string,
  cwd: string,
  absPath: string,
  entityName: string,
  signal?: AbortSignal,
): Promise<DependentsResult> {
  const result = await runCommand(
    semBin,
    ["impact", entityName, "--file", absPath, "--json", "--dependents", "--no-default-excludes"],
    cwd,
    signal,
  );

  if (result.exitCode !== 0) {
    return { ok: false, reason: result.stderr.trim() || `sem impact exited ${result.exitCode}` };
  }

  try {
    const parsed = JSON.parse(result.stdout) as { dependents?: DependentInfo[] };
    return { ok: true, dependents: parsed.dependents ?? [] };
  } catch (err) {
    return { ok: false, reason: `invalid JSON from sem impact: ${err instanceof Error ? err.message : String(err)}` };
  }
}
