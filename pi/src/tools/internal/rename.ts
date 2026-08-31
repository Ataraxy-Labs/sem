import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import type { Coordinator } from "./weave-coordination.ts";
import { extractEntities, type Entity } from "./entities.ts";
import { repoRelativePath } from "./git.ts";
import { runCommand } from "./proc.ts";
import { parseLines, renderLines } from "./text.ts";
import { performWeaveEdit, type WeaveEditParams, type MergeStatus } from "../weave-edit.ts";

/**
 * Rename engine: resolves one definition via `sem callers`, collects every
 * reference site via a word-boundary `sem grep`, applies the whole set as ONE
 * atomic weave_edit batch (each entity going through weave_edit's own full
 * claim/verify/release lifecycle), patches bare import lines directly, then
 * verifies with a leftover sweep that is deliberately INDEPENDENT of sem's
 * own index (`git ls-files` + a raw scan).
 *
 * Two empirically-confirmed facts about `sem` (against the real binary) are
 * load-bearing here — do not redesign around them:
 *
 * 1. `sem entities --json` does NOT list plain `import { x } from "...";`
 *    statements as entities at all. It DOES list a bare re-export
 *    (`export { x } from "...";`) as its own entity, `type: "export"`,
 *    spanning just that one line, and models `test("title", fn)` calls as
 *    entities named after the literal title string. So import lines have no
 *    enclosing entity (they get direct line patches, outside weave-mcp
 *    coordination), while re-export and test-title occurrences DO live inside
 *    real entities and go through the normal batch.
 *
 * 2. `sem callers <name> --json` groups by exact name across the whole repo,
 *    so a renamed-and-re-exported name returns TWO groups: the real
 *    definition plus the `type: "export"` re-export entity. Treating more
 *    than one group as blanket ambiguity would falsely refuse any rename of
 *    something re-exported anywhere — so definition candidates always exclude
 *    `type: "export"` groups (a re-export is a reference site, never a second
 *    independent definition).
 *
 * This module is an internal engine only — a different lane wires
 * `sem.rename(old, new)` on top of it in src/codemode. No tool registration
 * lives here.
 */

export interface RenameParams {
  /** Exact, case-sensitive current name of the entity to rename. */
  old_name: string;
  new_name: string;
  /** Disambiguates when old_name resolves to more than one entity across the repo — same role as sem_callers' entity_type, but scoped to a file since a rename target is usually unique per file. */
  file?: string;
  /** Coordinate every entity edit through weave-mcp's claim/release protocol. Defaults to true, same as weave_edit. */
  claim?: boolean;
}

export interface RenameDeps {
  cwd: string;
  semBin: string;
  coordinator: Coordinator | undefined;
  signal?: AbortSignal;
}

export interface RenameLeftover {
  file: string;
  line: number;
  snippet: string;
  /** True when the line looks like a comment or documentation/prose rather than code — e.g. starts with `//`/`#`/`*`, or the file is markdown/text. Editing these automatically isn't safe, so they're always surfaced rather than silently patched. */
  looksLikeCommentOrDoc: boolean;
}

export interface RenameOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

/** Mirrors sem-callers.ts's local shapes (not exported there). */
interface RawEntityRef {
  id?: string;
  name: string;
  type: string;
  file: string;
  start_line: number;
  end_line: number;
}

interface RawCallersGroup {
  entity: RawEntityRef;
  related: RawEntityRef[];
}

interface RawGrepHit {
  file: string;
  line: number;
  text: string;
}

/** One weave_edit batch item, typed off the real schema so drift fails tsc here. */
type WeaveEditItem = NonNullable<WeaveEditParams["edits"]>[number];

/** Defensive — identifiers won't normally contain metacharacters, but don't assume. */
function escapeRegExp(name: string): string {
  return name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function wordBoundaryPattern(name: string): string {
  return `\\b${escapeRegExp(name)}\\b`;
}

/** sem's entity ids are parent_id::name, recursively — duplicated locally from sem-callers.ts (not exported there). */
function parseParentNameFromId(id: string | undefined): string | null {
  if (id === undefined) return null;
  const parts = id.split("::");
  return parts.length > 3 ? (parts[parts.length - 2] ?? null) : null;
}

/** Same candidate-line shape as sem_callers.ts's formatCandidate, with sem_rename's own wording kept out (the header differs, the list shape is shared). */
function formatCandidate(entity: RawEntityRef): string {
  const parentName = parseParentNameFromId(entity.id);
  const parent = parentName ? ` in ${parentName}` : "";
  return `  - ${entity.name} (${entity.type}${parent}) — ${entity.file}:L${entity.start_line}-L${entity.end_line}`;
}

/**
 * Runs `sem callers <query> --json`. Any exit code is accepted as long as
 * stdout parses as JSON (the CLI exits 0 both for matches and for an empty
 * `[]`); only unparseable output throws.
 */
async function runSemCallersJson(query: string, deps: RenameDeps): Promise<RawCallersGroup[]> {
  const result = await runCommand(deps.semBin, ["callers", query, "--json"], deps.cwd, deps.signal);
  let groups: RawCallersGroup[];
  try {
    groups = JSON.parse(result.stdout) as RawCallersGroup[];
  } catch (err) {
    throw new Error(
      `"${deps.semBin} callers ${JSON.stringify(query)} --json" failed (exit ${result.exitCode}): ${
        result.stderr.trim() || (err instanceof Error ? err.message : String(err))
      }`,
    );
  }
  return Array.isArray(groups) ? groups : [];
}

/**
 * Runs `sem grep <pattern> --json`. Same convention as sem_grep's own runner:
 * exit 1 with `hits: []` is rg's no-match answer, NOT an error; only
 * unparseable output (crash, bad regex → exit 2) throws.
 */
async function runSemGrepJson(pattern: string, deps: RenameDeps): Promise<RawGrepHit[]> {
  const result = await runCommand(deps.semBin, ["grep", pattern, "--json"], deps.cwd, deps.signal);
  let parsed: { hits?: RawGrepHit[] };
  try {
    parsed = JSON.parse(result.stdout) as { hits?: RawGrepHit[] };
  } catch (err) {
    throw new Error(
      `"${deps.semBin} grep ${JSON.stringify(pattern)} --json" failed (exit ${result.exitCode}): ${
        result.stderr.trim() || (err instanceof Error ? err.message : String(err))
      }`,
    );
  }
  return Array.isArray(parsed?.hits) ? parsed.hits : [];
}

/**
 * The entity's current source as an interior slice: EOL-aware, WITHOUT a
 * trailing newline — weave_edit's splice splits content on line breaks, so a
 * trailing newline here would grow a spurious blank line mid-file.
 */
function sliceEntityText(content: string, startLine: number, endLine: number): string {
  const model = parseLines(content);
  return renderLines({ eol: model.eol, hasTrailingNewline: false }, model.lines.slice(startLine - 1, endLine));
}

export async function performRename(params: RenameParams, deps: RenameDeps): Promise<RenameOutcome> {
  const baseDetails = { old_name: params.old_name, new_name: params.new_name };
  const oldName = params.old_name;

  // ---- Step 1: resolve the definition ----
  let groups: RawCallersGroup[];
  try {
    groups = await runSemCallersJson(oldName, deps);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { isError: true, text: `sem_rename: ${message}`, details: { ...baseDetails, error: message } };
  }

  // A re-export is a reference site, never a second independent definition —
  // excluding its group here is what lets renaming a re-exported name work
  // at all (see the module doc, fact 2).
  const allDefCandidates = groups.filter((g) => g.entity.type !== "export");
  const requestedFile = params.file !== undefined && params.file.startsWith("./") ? params.file.slice(2) : params.file;
  const defCandidates =
    requestedFile !== undefined ? allDefCandidates.filter((g) => g.entity.file === requestedFile) : allDefCandidates;
  // Same-named definitions in OTHER files, dropped by the file= disambiguation
  // above: they are unrelated entities, not references to the target, so their
  // lines must be excluded from the rename set entirely (and surface as
  // leftovers after the sweep, honestly).
  const rejectedDefs = requestedFile !== undefined ? allDefCandidates.filter((g) => g.entity.file !== requestedFile) : [];

  if (defCandidates.length === 0) {
    return {
      isError: true,
      text: `sem_rename: no entity named "${params.old_name}" found. sem's matching is exact and case-sensitive — check the spelling, or use sem_find to confirm it exists.`,
      details: { ...baseDetails, resolved: false },
    };
  }

  if (defCandidates.length > 1) {
    const list = defCandidates.map((g) => formatCandidate(g.entity)).join("\n");
    return {
      isError: true,
      text: `sem_rename: "${params.old_name}" is ambiguous — ${defCandidates.length} matches. Pass file= to narrow it:\n${list}`,
      details: {
        ...baseDetails,
        resolved: false,
        candidates: defCandidates.map((g) => ({
          name: g.entity.name,
          type: g.entity.type,
          file: g.entity.file,
          parent_name: parseParentNameFromId(g.entity.id),
          start_line: g.entity.start_line,
          end_line: g.entity.end_line,
        })),
      },
    };
  }

  const definition = defCandidates[0]!.entity;

  // ---- Step 2: collect reference sites (word-boundary grep, whole repo) ----
  let grepHits: RawGrepHit[];
  try {
    grepHits = await runSemGrepJson(wordBoundaryPattern(oldName), deps);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { isError: true, text: `sem_rename: ${message}`, details: { ...baseDetails, error: message } };
  }

  // Deduped distinct (file,line) sites — this set's size is the applied count.
  const siteSet = new Map<string, { file: string; line: number }>();
  for (const hit of grepHits) siteSet.set(`${hit.file}:${hit.line}`, { file: hit.file, line: hit.line });

  // ---- Step 3: apply ----
  const entitiesCache = new Map<string, Entity[]>();
  const entitiesFor = async (file: string): Promise<Entity[]> => {
    const cached = entitiesCache.get(file);
    if (cached !== undefined) return cached;
    try {
      const extracted = await extractEntities(deps.semBin, resolve(deps.cwd, file), deps.cwd, deps.signal);
      entitiesCache.set(file, extracted);
      return extracted;
    } catch {
      // Unreadable/unindexable file: treat as having no entities, so its
      // sites fall through to direct line patches instead of failing the
      // whole rename.
      entitiesCache.set(file, []);
      return [];
    }
  };

  const contentCache = new Map<string, string>();
  const contentOf = async (file: string): Promise<string> => {
    const cached = contentCache.get(file);
    if (cached !== undefined) return cached;
    const content = await readFile(resolve(deps.cwd, file), "utf8");
    contentCache.set(file, content);
    return content;
  };

  const rejectedInFile = (entity: Entity, file: string): boolean =>
    rejectedDefs.some(
      (r) =>
        r.entity.file === file &&
        r.entity.name === entity.name &&
        r.entity.type === entity.type &&
        r.entity.start_line === entity.start_line &&
        r.entity.end_line === entity.end_line,
    );

  // Group each site by its innermost enclosing entity. Only the ENTITY SET
  // matters — each entity's whole slice gets a global word-boundary replace,
  // so every site inside it is fixed by construction; the sites merely pick
  // which entities to edit. Sites with no enclosing entity (bare import
  // lines — see fact 1) are patched line-wise after the batch lands.
  const groupEntities = new Map<string, { file: string; entity: Entity }>();
  const directPatchSites: Array<{ file: string; line: number }> = [];

  for (const site of siteSet.values()) {
    // Inside the definition's own range: covered by the definition's own
    // batch item below — never a separate group.
    if (site.file === definition.file && site.line >= definition.start_line && site.line <= definition.end_line) continue;

    const entities = await entitiesFor(site.file);
    let innermost: Entity | undefined;
    for (const entity of entities) {
      if (site.line < entity.start_line || site.line > entity.end_line) continue;
      if (!innermost || entity.end_line - entity.start_line < innermost.end_line - innermost.start_line) innermost = entity;
    }

    if (innermost === undefined) {
      directPatchSites.push(site);
      continue;
    }

    // An unrelated same-named definition in another file (dropped by the
    // file= disambiguation) is not a reference site — leave both the site
    // and its entity completely alone.
    if (rejectedInFile(innermost, site.file)) {
      siteSet.delete(`${site.file}:${site.line}`);
      continue;
    }

    const key = `${site.file}:${innermost.start_line}:${innermost.end_line}:${innermost.type}:${innermost.name}`;
    if (!groupEntities.has(key)) groupEntities.set(key, { file: site.file, entity: innermost });
  }

  const renamed = (text: string): string => text.replace(new RegExp(wordBoundaryPattern(oldName), "g"), params.new_name);
  // True iff the entity's OWN name carries the old name (definition itself,
  // "export"-typed re-export entities, "test"-typed titles mentioning it) —
  // such a replace changes identity, which weave_edit refuses without this.
  const touchesOwnName = (name: string): boolean => new RegExp(wordBoundaryPattern(oldName)).test(name);

  const defContent = await contentOf(definition.file);
  const defItem: WeaveEditItem = {
    file: definition.file,
    entity: {
      name: definition.name,
      entity_type: definition.type,
      parent_name: parseParentNameFromId(definition.id) ?? undefined,
    },
    op: "replace",
    content: renamed(sliceEntityText(defContent, definition.start_line, definition.end_line)),
    allow_signature_change: touchesOwnName(definition.name),
  };

  const entityItems: WeaveEditItem[] = [];
  for (const { file, entity } of groupEntities.values()) {
    entityItems.push({
      file,
      entity: { name: entity.name, entity_type: entity.type, parent_name: entity.parentName ?? undefined },
      op: "replace",
      content: renamed(sliceEntityText(await contentOf(file), entity.start_line, entity.end_line)),
      allow_signature_change: touchesOwnName(entity.name),
    });
  }

  // One atomic batch: if ANY entity edit fails, everything rolls back on disk
  // (and weave-mcp claims get resynced) — nothing else may be touched after
  // that, so rename stops right here.
  const batchOutcome = await performWeaveEdit(
    { edits: [defItem, ...entityItems], atomic: true, claim: params.claim },
    deps,
  );
  // Aggregate the per-entity merge backstop outcomes into ONE MergeStatus
  // for the whole rename (same shape sem.edit() reports -- one type, not a
  // rename-specific variant): attempted/performed if ANY entity edit was,
  // driftDetected if any saw drift, mergedOver the union minus this
  // rename's own touched entities.
  const entryMerges = ((batchOutcome.details as { results?: Array<{ details?: { merge?: MergeStatus } }> }).results ?? [])
    .map((r) => r.details?.merge)
    .filter((m): m is MergeStatus => m !== undefined);
  const ownEntityNames = new Set([defItem, ...entityItems].flatMap((e) => [e.entity.name, params.new_name]));
  const driftSeen = entryMerges.some((m) => m.driftDetected === true);
  const merge: MergeStatus = {
    attempted: entryMerges.some((m) => m.attempted),
    performed: entryMerges.some((m) => m.performed),
    ...(entryMerges.some((m) => m.driftDetected !== undefined) ? { driftDetected: driftSeen } : {}),
    ...(entryMerges.some((m) => m.performed)
      ? { mergedOver: [...new Set(entryMerges.flatMap((m) => m.mergedOver ?? []))].filter((n) => !ownEntityNames.has(n)).sort() }
      : {}),
  };
  if (batchOutcome.isError) {
    return {
      isError: true,
      text: `sem_rename: failed to apply: ${batchOutcome.text}`,
      details: { ...baseDetails, refused: batchOutcome.text },
    };
  }

  // Batch succeeded — now the bare import lines: no entity to claim, so these
  // bypass weave-mcp coordination entirely. Best-effort: a failed patch isn't
  // counted as touched, and the leftover sweep reports it honestly.
  const patchedFiles = new Set<string>();
  for (const site of directPatchSites) {
    try {
      const model = parseLines(await readFile(resolve(deps.cwd, site.file), "utf8"));
      const index = site.line - 1;
      const line = model.lines[index];
      if (line === undefined) continue;
      const replaced = line.replace(new RegExp(wordBoundaryPattern(oldName), "g"), params.new_name);
      if (replaced === line) continue;
      model.lines[index] = replaced;
      await writeFile(resolve(deps.cwd, site.file), renderLines(model, model.lines), "utf8");
      patchedFiles.add(site.file);
    } catch {
      // Deliberately swallowed — the sweep below catches this occurrence.
    }
  }

  // ---- Step 4: leftover sweep (independent of sem's own index) ----
  // `sem grep` only sees files sem indexes (a sibling .txt next to indexed
  // .ts files is invisible to it — 0 hits, not even in candidate_files), so
  // verification must NOT reuse it. Sweep every tracked file instead; when
  // this isn't a git repo, skip quietly — coordination-adjacent code here
  // never throws for a missing git repo.
  const leftovers: RenameLeftover[] = [];
  try {
    const repoLoc = await repoRelativePath(resolve(deps.cwd, "."));
    const repoRoot = repoLoc?.root ?? deps.cwd;
    const lsResult = await runCommand("git", ["ls-files"], repoRoot, deps.signal);
    if (lsResult.exitCode === 0) {
      const trackedFiles = lsResult.stdout
        .split("\n")
        .map((entry) => entry.trim())
        .filter((entry) => entry.length > 0);
      const leftoverTest = new RegExp(wordBoundaryPattern(oldName));
      const isDocFile = (file: string): boolean =>
        [".md", ".txt", ".rst", ".adoc"].some((ext) => file.toLowerCase().endsWith(ext));

      for (const tracked of trackedFiles) {
        // Defensively skip node_modules/.git even though ls-files shouldn't list them.
        if (
          tracked.startsWith("node_modules/") ||
          tracked.startsWith(".git/") ||
          tracked.includes("/node_modules/") ||
          tracked.includes("/.git/")
        ) {
          continue;
        }

        let content: string;
        try {
          content = await readFile(resolve(repoRoot, tracked), "utf8");
        } catch {
          continue; // unreadable file: skip quietly
        }

        const lines = content.split("\n");
        for (let i = 0; i < lines.length; i++) {
          const lineText = lines[i]!;
          if (!leftoverTest.test(lineText)) continue;
          const trimmed = lineText.trim();
          leftovers.push({
            file: tracked.split("\\").join("/"),
            line: i + 1,
            snippet: trimmed,
            looksLikeCommentOrDoc:
              trimmed.startsWith("//") ||
              trimmed.startsWith("#") ||
              trimmed.startsWith("*") ||
              trimmed.startsWith("/*") ||
              isDocFile(tracked),
          });
        }
      }
    }
  } catch {
    // No git / git failed: sweep skipped — treated as verified with no leftovers.
  }

  // ---- Step 5: shape the result ----
  const applied = siteSet.size;
  const files = Array.from(new Set([...[defItem, ...entityItems].map((edit) => edit.file), ...patchedFiles])).sort();
  const verified = leftovers.length === 0;

  // The batch's FIRST entry is `defItem` -- the DEFINITION itself -- and
  // weave_edit already captured its dependents before writing (that check is
  // unconditional for a `replace`, see weave-edit.ts's `dependentsBefore`).
  // Threading that one report out costs nothing and is exactly "who
  // referenced the entity being renamed", which api.ts renders as the
  // rename receipt's one-line `impact`. Left in weave_edit's own
  // DependentsReport shape rather than pre-formatted here: rename.ts is
  // shared with tools-mode, and the code-mode wording lives in api.ts.
  const definitionDependents = ((batchOutcome.details as { results?: Array<{ details?: { dependents?: unknown } }> }).results ?? [])[0]?.details
    ?.dependents;

  const sitePlural = applied === 1 ? "site" : "sites";
  const filePlural = files.length === 1 ? "file" : "files";
  // The honesty note: a rename that merged over someone's concurrent work
  // says so in its one-line text, naming what changed underneath.
  const mergeNote =
    merge.performed && driftSeen ? `, merged over concurrent changes to ${(merge.mergedOver ?? []).join(", ") || "other entities"}` : "";
  const text = verified
    ? `${applied} ${sitePlural} in ${files.length} ${filePlural}, verified, 0 leftovers${mergeNote}`
    : `${applied} ${sitePlural} in ${files.length} ${filePlural}, ${leftovers.length} leftover${
        leftovers.length === 1 ? "" : "s"
      } remain — see details.leftovers${mergeNote}`;

  return {
    isError: false,
    text,
    details: { ...baseDetails, applied, files, verified, leftovers, merge, dependents: definitionDependents },
  };
}
