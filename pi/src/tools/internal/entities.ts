import { mkdtemp, rmdir, unlink, writeFile } from "node:fs/promises";
import { basename, join } from "node:path";
import { tmpdir } from "node:os";
import { runCommand } from "./proc.ts";

/**
 * Entity extraction and disambiguation, backed by `sem entities --json`.
 *
 * Chosen over weave-mcp's weave_extract_entities: both were measured against
 * a 2200-line fixture. The `sem` CLI answers in ~15ms with no process to
 * manage, and its JSON includes byte ranges plus a `parent_id` that already
 * carries the parent's name as its final `::`-segment — exactly what entity
 * disambiguation needs. weave_extract_entities requires a full MCP
 * handshake per call (~30ms observed) and returns line ranges only. sem's
 * CLI wins on both latency and the shape of the data; weave-mcp is reserved
 * for the live coordination tools (claim/register/update/release), where a
 * persistent process is worth it.
 */

export interface RawEntity {
  name: string;
  type: string;
  start_line: number;
  end_line: number;
  /** sem omits these for non-code entities (markdown headings, toml/yaml sections, ...); never assume they're present. */
  start_byte?: number;
  end_byte?: number;
  parent_id: string | null;
}

export interface Entity extends RawEntity {
  parentName: string | null;
  /** True iff sem returned a usable byte range for this entity (both bytes present as numbers and not 0/0). */
  byteRangeReliable: boolean;
}

export interface EntityQuery {
  name: string;
  entity_type?: string;
  parent_name?: string;
  ordinal?: number;
}

export type ResolveResult =
  | { kind: "found"; entity: Entity }
  | {
      kind: "not-found";
      nearest: string[];
      /**
       * The entities that DID match the name, when an entity_type/parent_name
       * filter is what emptied the set -- absent only when the name genuinely
       * matches nothing. Carries the answer the caller was actually looking
       * for, so the refusal can name the real type/parent instead of
       * reporting "no entity named X" and then offering X as the closest
       * name (43 such self-contradictions across 39 runs in the 2026-09-02
       * transcript study).
       */
      filteredOut?: Entity[];
      /** The filter values that emptied it, echoed back so the message can name them. */
      filters?: { entity_type?: string; parent_name?: string };
    }
  | {
      kind: "ambiguous";
      candidates: Entity[];
      /**
       * True when an `ordinal` was supplied and DELIBERATELY not honoured
       * because `parent_name` separates these candidates one-to-one -- see
       * resolveEntity. The caller renders a different refusal for this: the
       * fix is a parent_name, not a better index.
       */
      ordinalRefused?: boolean;
    };

function parentNameOf(parentId: string | null): string | null {
  if (!parentId) return null;
  const segments = parentId.split("::");
  const last = segments[segments.length - 1];
  return last && last.length > 0 ? last : null;
}

/**
 * The raw-JSON-to-Entity mapping, isolated from the `sem entities` shell-out
 * so it's directly testable against captured JSON shapes instead of
 * whatever a live, version-specific `sem` binary happens to return right
 * now. `sem` versions disagree on whether non-code entities (markdown
 * headings, toml/yaml sections, ...) carry a byte range at all — sem 0.23.0
 * omits start_byte/end_byte for them entirely; sem 0.23.1+ includes one
 * (confirmed empirically against both). byteRangeReliable is the single
 * flag every consumer (sem-outline's token estimate, weave-edit's splice
 * safety) keys off of instead of caring which shape produced it.
 */
export function deriveEntity(raw: RawEntity): Entity {
  return {
    ...raw,
    parentName: parentNameOf(raw.parent_id),
    byteRangeReliable: typeof raw.start_byte === "number" && typeof raw.end_byte === "number" && !(raw.start_byte === 0 && raw.end_byte === 0),
  };
}

export async function extractEntities(
  semBin: string,
  absPath: string,
  cwd: string,
  signal?: AbortSignal,
): Promise<Entity[]> {
  const result = await runCommand(semBin, ["entities", "--json", absPath], cwd, signal);
  if (result.exitCode !== 0) {
    throw new Error(
      `"${semBin} entities --json ${absPath}" failed (exit ${result.exitCode}): ${result.stderr.trim() || "no stderr output"}`,
    );
  }

  let raw: RawEntity[];
  try {
    raw = JSON.parse(result.stdout) as RawEntity[];
  } catch (err) {
    throw new Error(
      `"${semBin} entities --json ${absPath}" produced invalid JSON: ${err instanceof Error ? err.message : String(err)}`,
    );
  }

  return raw.map(deriveEntity);
}

/**
 * Entity extraction from a text the caller ALREADY HOLDS, rather than from
 * whatever is on disk at `absPath` right now.
 *
 * This exists so weave-edit's post-write verification can be a PURE function
 * of the transaction's own output. Verification asks "does the text this
 * edit produced still parse cleanly around the change?" -- a question about
 * `textToWrite`, not about disk. Answering it with a disk re-read made it
 * answerable by a *stranger*: a foreign process writing the same file while
 * `sem entities` read it yields a torn parse, entities vanish from the
 * extraction, and verifyEdit reports a false "untouched entities are gone"
 * verdict about a file state that never logically existed. That false
 * verdict then drove a rollback. Extracting from the buffer removes the
 * whole failure mode at the source rather than defending against it.
 *
 * `sem entities --json` takes a path, not stdin, so the buffer is staged in
 * a private temp directory. The BASENAME is preserved, not just the
 * extension: sem selects a language from the file name, and names with no
 * extension (Makefile, Dockerfile, go.mod) carry their language in the
 * basename alone. Extraction is otherwise path-independent -- verified
 * empirically: the same bytes under a repo path and under a tmpdir path give
 * byte-identical JSON, line ranges, byte ranges and parent ids included.
 *
 * The temp directory is created by mkdtemp and removed by unlink+rmdir (no
 * recursive removal, ever) so a crash between the two leaves one stray file
 * in the OS temp dir and nothing else.
 */
export async function extractEntitiesFromText(
  semBin: string,
  text: string,
  absPath: string,
  cwd: string,
  signal?: AbortSignal,
): Promise<Entity[]> {
  const scratchDir = await mkdtemp(join(tmpdir(), "pi-sem-verify-"));
  const scratchPath = join(scratchDir, basename(absPath));
  try {
    await writeFile(scratchPath, text, "utf8");
    return await extractEntities(semBin, scratchPath, cwd, signal);
  } catch (err) {
    // The staging path is an implementation detail; a caller debugging a sem
    // failure needs the file it actually asked about.
    const message = err instanceof Error ? err.message : String(err);
    throw new Error(message.split(scratchPath).join(`${absPath} (staged copy)`));
  } finally {
    await unlink(scratchPath).catch(() => {});
    await rmdir(scratchDir).catch(() => {});
  }
}

function levenshtein(a: string, b: string): number {
  const dp: number[] = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    let prevDiagonal = dp[0]!;
    dp[0] = i;
    for (let j = 1; j <= b.length; j++) {
      const upLeft = prevDiagonal;
      prevDiagonal = dp[j]!;
      dp[j] = a[i - 1] === b[j - 1] ? upLeft : 1 + Math.min(upLeft, dp[j]!, dp[j - 1]!);
    }
  }
  return dp[b.length]!;
}

export function nearestNames(entities: Entity[], name: string, limit = 5): string[] {
  const distinct = Array.from(new Set(entities.map((e) => e.name)));
  const threshold = Math.max(3, Math.ceil(name.length / 2));
  return distinct
    .map((candidate) => ({ candidate, distance: levenshtein(name, candidate) }))
    .filter((entry) => entry.distance <= threshold)
    .sort((a, b) => a.distance - b.distance)
    .slice(0, limit)
    .map((entry) => entry.candidate);
}

/**
 * Renders the filter values that emptied a name match, for the P4b refusal
 * ("... but none match {entity_type:\"method\"}"). Shared by sem_read and
 * weave_edit so the two tools cannot drift into describing the same refusal
 * differently.
 */
export function describeEntityFilters(filters?: { entity_type?: string; parent_name?: string }): string {
  const parts: string[] = [];
  if (filters?.entity_type !== undefined) parts.push(`entity_type:"${filters.entity_type}"`);
  if (filters?.parent_name !== undefined) parts.push(`parent_name:"${filters.parent_name}"`);
  return `{${parts.join(", ")}}`;
}

/** "1 entity named \"x\" exists" / "3 entities named \"x\" exist" -- the subject of the P4b refusal sentence. */
export function describeNameMatchCount(name: string, count: number): string {
  return count === 1 ? `1 entity named "${name}" exists` : `${count} entities named "${name}" exist`;
}

/**
 * True when every candidate carries a DISTINCT parent name -- i.e. a
 * `parent_name` would have selected exactly one of them. That is the
 * precondition for refusing a bare `ordinal` (P4a): an ordinal indexes a
 * start-line-sorted list, so it silently re-points at a different entity
 * whenever the file grows a definition above the one that was meant, and
 * django__django-13128 shows what that costs -- `ordinal:1` overwrote
 * `ResolvedOuterRef.resolve_expression` with `CombinedExpression` logic and
 * never erred. When parents do NOT separate the candidates (two overloads
 * under the same parent, two module-level definitions), an ordinal is
 * genuinely the last resort it is documented to be, and is still honoured.
 */
function parentNameWouldDisambiguate(candidates: Entity[]): boolean {
  const parents = new Set(candidates.map((e) => e.parentName ?? ""));
  return parents.size === candidates.length && !parents.has("");
}

/**
 * Applies the same disambiguation rule weave-mcp's own entity resolution
 * uses: filter by name, then type, then parent, then ordinal. Zero matches
 * is reported with the nearest names in the file; more than one is refused
 * with the full candidate list — this never guesses by picking the first.
 *
 * Two honesty guarantees on top of that (P4, 2026-09-02 transcript study):
 * a `not-found` produced by the type/parent FILTER carries the name matches
 * it discarded, so the caller never says "no entity named X" and then
 * offers X as the nearest name; and a bare `ordinal` is refused rather than
 * honoured when `parent_name` would have separated the candidates
 * one-to-one.
 */
export function resolveEntity(entities: Entity[], query: EntityQuery): ResolveResult {
  const byName = entities.filter((e) => e.name === query.name);
  let candidates = byName;
  if (query.entity_type) candidates = candidates.filter((e) => e.type === query.entity_type);
  if (query.parent_name) candidates = candidates.filter((e) => e.parentName === query.parent_name);

  if (candidates.length === 0) {
    if (byName.length > 0) {
      return {
        kind: "not-found",
        nearest: [],
        filteredOut: [...byName].sort((a, b) => a.start_line - b.start_line),
        filters: { entity_type: query.entity_type, parent_name: query.parent_name },
      };
    }
    return { kind: "not-found", nearest: nearestNames(entities, query.name) };
  }

  const sorted = [...candidates].sort((a, b) => a.start_line - b.start_line);

  if (sorted.length === 1) {
    return { kind: "found", entity: sorted[0]! };
  }

  if (query.ordinal !== undefined) {
    if (query.parent_name === undefined && parentNameWouldDisambiguate(sorted)) {
      return { kind: "ambiguous", candidates: sorted, ordinalRefused: true };
    }
    const picked = sorted[query.ordinal];
    if (picked) return { kind: "found", entity: picked };
  }

  return { kind: "ambiguous", candidates: sorted };
}
