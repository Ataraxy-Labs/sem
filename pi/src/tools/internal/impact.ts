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
  /**
   * The entity's own stable sem id (`parent_id::name`, or
   * `<repo-relative file>::<type>::<name>` at module level), when the caller
   * has ALREADY resolved it.
   *
   * P4c of the 2026-09-02 transcript study: this check used to re-resolve by
   * BARE NAME, throwing away the entity_type/parent_name/ordinal that had
   * just resolved the edit -- so `sem impact` answered "Entity name
   * 'deconstruct' is ambiguous (5 matches)" for an edit that had landed
   * perfectly, 33 times across 30 runs, and django__django-10914 read that
   * as "the edit didn't land" and retried. `sem impact --entity-id` asks the
   * question the caller actually has the answer to. Falls back to the
   * name-based lookup if the id lookup fails, so an id this code built
   * wrongly degrades to the old behaviour instead of losing the check.
   */
  entityId?: string,
): Promise<DependentsResult> {
  if (entityId !== undefined) {
    const byId = await runCommand(
      semBin,
      ["impact", "--entity-id", entityId, "--json", "--dependents", "--no-default-excludes"],
      cwd,
      signal,
    );
    if (byId.exitCode === 0) return parseDependents(byId.stdout);
  }

  const result = await runCommand(
    semBin,
    ["impact", entityName, "--file", absPath, "--json", "--dependents", "--no-default-excludes"],
    cwd,
    signal,
  );

  if (result.exitCode !== 0) {
    return { ok: false, reason: result.stderr.trim() || `sem impact exited ${result.exitCode}` };
  }

  return parseDependents(result.stdout);
}

function parseDependents(stdout: string): DependentsResult {
  try {
    const parsed = JSON.parse(stdout) as { dependents?: DependentInfo[] };
    return { ok: true, dependents: parsed.dependents ?? [] };
  } catch (err) {
    return { ok: false, reason: `invalid JSON from sem impact: ${err instanceof Error ? err.message : String(err)}` };
  }
}
