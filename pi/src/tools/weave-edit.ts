import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { withFileMutationQueue } from "@earendil-works/pi-coding-agent";
import { readFile } from "node:fs/promises";
// Sync twins, used ONLY inside guardedWrite -- see the HONEST RESIDUAL
// comment there for why the async pair is measurably not narrow enough.
import { readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { isAbsolute, relative, resolve } from "node:path";
import { Type, type Static } from "typebox";
import { currentBranch, repoRelativePath, type RepoLocation } from "./internal/git.ts";
import { describeEntityFilters, describeNameMatchCount, extractEntities, extractEntitiesFromText, resolveEntity, type Entity, type ResolveResult } from "./internal/entities.ts";
import { splice, parseLines, renderLines, type Op } from "./internal/text.ts";
import { verifyEdit } from "./internal/verify.ts";
import { compareIdentity, deriveVisibility, type IdentityChange, type IdentityFacts } from "./internal/identity.ts";
import { checkDependents, type DependentInfo } from "./internal/impact.ts";
import { Coordinator, type MergeConflictSummary } from "./internal/weave-coordination.ts";

const EntityRefSchema = Type.Object({
  name: Type.String({ description: "Exact name of the function, class, method, etc. to target." }),
  entity_type: Type.Optional(
    Type.String({
      description:
        "Entity kind to disambiguate when the name exists as more than one kind, e.g. 'function', 'class', 'method', 'variable' (language-dependent; matches sem's `type` field).",
    }),
  ),
  parent_name: Type.Optional(
    Type.String({
      description: "Name of the enclosing entity (e.g. the class owning a method) to disambiguate when the name exists in more than one place.",
    }),
  ),
  ordinal: Type.Optional(
    Type.Integer({
      minimum: 0,
      description:
        "0-based occurrence index, in file order, among matches still ambiguous after entity_type/parent_name. Genuine last resort: REFUSED (with the candidate list) when the candidates each sit under a different parent, since parent_name addresses them stably and an ordinal silently re-points when the file changes.",
    }),
  ),
});

const OpSchema = Type.Union(
  [Type.Literal("replace"), Type.Literal("insert_after"), Type.Literal("insert_before"), Type.Literal("delete")],
  {
    description:
      "replace: swap the entity's full source for `content`. insert_after / insert_before: add `content` as a new entity immediately after/before this one. delete: remove the entity.",
  },
);

const WeaveEditItemSchema = Type.Object({
  file: Type.String({ description: "Path to the file containing the entity, relative to the working directory or absolute." }),
  entity: EntityRefSchema,
  op: OpSchema,
  content: Type.Optional(
    Type.String({
      description: "Full new source of the entity — its signature, body, and closing brace — for replace/insert_after/insert_before. Omit for delete.",
    }),
  ),
  allow_signature_change: Type.Optional(
    Type.Boolean({
      description:
        "weave_edit refuses a replace that changes a function's exported-ness or its name/kind/parent, and rolls it back, unless this is true. Set it only when the visibility or identity change is intentional.",
    }),
  ),
});

const WeaveEditParamsSchema = Type.Object({
  file: Type.Optional(Type.String({ description: "Path to the file containing the entity, relative to the working directory or absolute. Required unless edits= is used." })),
  entity: Type.Optional(EntityRefSchema),
  op: Type.Optional(OpSchema),
  content: Type.Optional(
    Type.String({
      description: "Full new source of the entity — its signature, body, and closing brace — for replace/insert_after/insert_before. Omit for delete.",
    }),
  ),
  claim: Type.Optional(
    Type.Boolean({
      description: "Coordinate the edit through weave-mcp's live claim/release protocol so other agents editing this repo see it. Defaults to true; set false to skip coordination.",
    }),
  ),
  allow_signature_change: Type.Optional(
    Type.Boolean({
      description:
        "weave_edit refuses a replace that changes a function's exported-ness or its name/kind/parent, and rolls it back, unless this is true. Set it only when the visibility or identity change is intentional.",
    }),
  ),
  edits: Type.Optional(
    Type.Array(WeaveEditItemSchema, {
      description:
        "Edit several entities (each its own {file, entity, op, content?, allow_signature_change?}) in one call instead of one call per entity, across one or many files. Each edit succeeds/fails independently unless atomic=true. Takes priority over file=/entity=/op= when given.",
    }),
  ),
  atomic: Type.Optional(
    Type.Boolean({
      description: "With edits=, roll back every edit in the batch if any one fails (all-or-nothing). Defaults to false — each edit succeeds/fails independently.",
    }),
  ),
});

export type WeaveEditParams = Static<typeof WeaveEditParamsSchema>;
export type EntityRef = Static<typeof EntityRefSchema>;

/** Single-entity params with file/entity/op guaranteed present — what the actual edit engine (performOneWeaveEdit) operates on, after the batch-vs-single dispatch in performWeaveEdit has resolved which one this is. */
export interface OneWeaveEditParams {
  file: string;
  entity: EntityRef;
  op: Op;
  content?: string;
  claim?: boolean;
  allow_signature_change?: boolean;
}

export interface CoordinationStatus {
  attempted: boolean;
  claimed: boolean;
  updated?: boolean;
  released?: boolean;
  skippedReason?: string;
  releaseError?: string;
}

/**
 * The merge backstop's caller-visible outcome for one edit. `attempted:
 * false, performed: false` is the explicit uncoordinated shape (no
 * coordinator, not a git repo, or claim=false opt-out) -- never absent, so
 * a reader can distinguish "no backstop ran" from "backstop ran, no
 * drift" without knowing the config. `performed: true` means another
 * agent's concurrent changes were actually merged over: driftDetected is
 * true and mergedOver names what changed underneath this edit.
 */
export interface MergeStatus {
  attempted: boolean;
  performed: boolean;
  driftDetected?: boolean;
  mergedOver?: string[];
}

export interface WeaveEditDeps {
  cwd: string;
  semBin: string;
  coordinator: Coordinator | undefined;
  signal?: AbortSignal;
}

export interface WeaveEditOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

export interface DependentsReport {
  checked: boolean;
  reason?: string;
  before?: DependentInfo[];
  after?: DependentInfo[];
  /** The before-check succeeded but the after-check itself failed (distinct from "after" legitimately coming back empty). */
  afterCheckFailed?: boolean;
}

function resolveTargetPath(cwd: string, file: string): string {
  const stripped = file.startsWith("@") ? file.slice(1) : file;
  return resolve(cwd, stripped);
}

/** A guarded write either landed, or lost its frame to a writer this engine did not control -- in which case nothing was written and the bytes that ARE there come back for the receipt. */
export type GuardedWriteResult = { kind: "written" } | { kind: "lost"; actualImage: string };

/**
 * THE write primitive of this engine. Every byte weave_edit ever puts on
 * disk goes through here -- the forward commit inside the merge-gate retry
 * loop, the forward commit on the uncoordinated path, the verification
 * rollback, the identity-refusal restore, and the atomic batch's snapshot
 * restore. `expectedImage` is what the caller believes is on disk; the write
 * happens only if that is still true.
 *
 * WHY IT IS ONE PRIMITIVE. The engine is optimistic concurrency control with
 * backward validation (Kung & Robinson 1981): read phase, validation phase,
 * write phase, retry on validation failure. Before this extraction only the
 * COMMIT was validated -- the compensating writes (Garcia-Molina & Salem
 * 1987's C(T)) were blind last-writer-wins restores of a snapshot taken
 * before the edit began, so a foreign process's COMPLETED write landing in
 * the verification window was destroyed with no error, no conflict and no
 * receipt anywhere. The transaction was asymmetric: validated commit,
 * unvalidated abort. Routing every write through one compare-adjacent-write
 * makes it symmetric, and states the rule once instead of at four sites:
 * this engine may only overwrite bytes it can prove are the bytes it
 * expected -- C(T) compensates T's own write, never a stranger's.
 *
 * WHY SYNC, AND WHY ADJACENT. The compare and the write are
 * readFileSync/writeFileSync, deliberately, and deliberately adjacent. The
 * async pair was measured and is NOT good enough: `await readFile` then
 * `await writeFile` puts a full event-loop turn plus two threadpool hops
 * between the check and the write -- a window on the order of a millisecond,
 * which under two lockstep agents still lost roughly one edit in forty. Two
 * adjacent sync syscalls in one uninterruptible JS turn is the narrowest
 * Node offers. The cost is that this one write blocks the event loop; the
 * callers are already inside withFileMutationQueue, so nothing else in this
 * process was going to touch the file anyway.
 *
 * HONEST RESIDUAL: this shrinks the window, it does not close it. Another OS
 * process's write can still land between these two syscalls and be
 * clobbered, and a writer that changed the file and changed it back (an
 * A-B-A) reads as unchanged to a content compare. Node offers no
 * compare-and-swap on a file, and the sole-writer discipline (weave never
 * writes files) rules out doing the swap server-side. What used to be a full
 * MCP round trip of exposure (tens of ms, hit on 80-100% of concurrent runs)
 * is now the microseconds between two syscalls with a string comparison
 * between them -- smaller by orders of magnitude, and not zero. A file lock
 * would close it and is deliberately not taken: a lock is a cross-tool
 * contract this package cannot enforce on the other editors sharing the same
 * checkout.
 */
function guardedWrite(absPath: string, expectedImage: string, bytes: string): GuardedWriteResult {
  const actualImage = readFileSync(absPath, "utf8");
  if (actualImage !== expectedImage) return { kind: "lost", actualImage };
  writeFileSync(absPath, bytes, "utf8");
  return { kind: "written" };
}

/** How a lost guard describes an image in a receipt: size and content hash, never the bytes. Result text and details are token-budgeted here (see MAX_INLINE_DEPENDENT_NAMES), and a caller only needs "these are not the same file" plus something citable. */
export interface ImageSummary {
  bytes: number;
  sha1: string;
}

function summarizeImage(text: string): ImageSummary {
  return { bytes: Buffer.byteLength(text, "utf8"), sha1: createHash("sha1").update(text, "utf8").digest("hex") };
}

/** Header + this many candidates + one trailing count line keeps every resolution refusal within ~6 lines no matter how ambiguous the name is; the complete list stays in details.candidates. */
const MAX_INLINE_CANDIDATES = 4;

/**
 * The entity's own stable sem id, built from what extractEntities already
 * returned -- zero extra lookups. `parent_id` IS the parent's own id (sem
 * gives it directly, always repo-relative), so a nested entity only appends
 * its own name; a module-level one is keyed by its file, type and name. Same
 * construction sem-read.ts's ownEntityId uses. Threaded into checkDependents
 * so the post-edit impact question is asked about the entity that was
 * ACTUALLY resolved rather than re-resolved from a bare name -- see P4c in
 * internal/impact.ts.
 */
function ownEntityId(entity: Entity, cwd: string, absPath: string): string {
  const relPath = isAbsolute(absPath) ? relative(cwd, absPath) : absPath;
  const parentId = entity.parent_id;
  if (!parentId) return `${relPath}::${entity.type}::${entity.name}`;
  // `sem entities` echoes back whatever path it was HANDED as the id's file
  // prefix (this file always hands it an absolute one), while `sem impact
  // --entity-id` keys its index by the repo-relative path. Re-prefix rather
  // than trusting the echo, or every nested entity's id misses.
  for (const prefix of [absPath, relPath]) {
    if (parentId.startsWith(`${prefix}::`)) return `${relPath}${parentId.slice(prefix.length)}::${entity.name}`;
  }
  return `${parentId}::${entity.name}`;
}

function formatCandidate(e: Entity): string {
  const parent = e.parentName ? ` in ${e.parentName}` : "";
  return `  - ${e.name} (${e.type}${parent}), lines ${e.start_line}-${e.end_line}`;
}

/** The machine-readable twin of formatCandidate, for `details.candidates`. */
function candidateSummary(e: Entity): { name: string; type: string; parent_name: string | null; start_line: number; end_line: number } {
  return { name: e.name, type: e.type, parent_name: e.parentName, start_line: e.start_line, end_line: e.end_line };
}

function formatResolutionFailure(
  params: OneWeaveEditParams,
  result: Extract<ResolveResult, { kind: "not-found" | "ambiguous" }>,
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  if (result.kind === "not-found") {
    // P4b: an entity_type/parent_name filter emptied a NON-empty name match.
    // Reporting "no entity named X" and then listing X as the closest name
    // is the self-contradiction 39 of 327 drive runs hit; name the filter
    // and show what the file really holds under that name.
    if (result.filteredOut !== undefined && result.filteredOut.length > 0) {
      const filters = describeEntityFilters(result.filters);
      const shownMatches = result.filteredOut.slice(0, MAX_INLINE_CANDIDATES);
      const omittedMatches = result.filteredOut.length - shownMatches.length;
      const matchList = shownMatches.map(formatCandidate).join("\n");
      const matchMore = omittedMatches > 0 ? `\n…${omittedMatches} more — full list in details.candidates` : "";
      return {
        isError: true,
        text: `weave_edit: ${describeNameMatchCount(params.entity.name, result.filteredOut.length)} in ${params.file}, but none match ${filters} — drop the filter, or use one of these:\n${matchList}${matchMore}`,
        details: {
          file: params.file,
          entity: params.entity,
          resolved: false,
          nearest: [],
          filters: result.filters ?? {},
          candidates: result.filteredOut.map(candidateSummary),
          coordination,
          merge,
        },
      };
    }
    const nearest = result.nearest.length > 0 ? ` Closest names in ${params.file}: ${result.nearest.join(", ")}.` : "";
    return {
      isError: true,
      text: `weave_edit: no entity named "${params.entity.name}" found in ${params.file}.${nearest}`,
      details: { file: params.file, entity: params.entity, resolved: false, nearest: result.nearest, coordination, merge },
    };
  }

  // Cap what gets inlined into text: listing every candidate made result text
  // grow unboundedly with ambiguity (an eval showed pi-sem costing far more
  // tokens per turn than vanilla pi). Header + 4 candidates + one trailing
  // count line stays within 6 lines no matter how many matches exist; the
  // complete list remains in details.candidates below.
  const shown = result.candidates.slice(0, MAX_INLINE_CANDIDATES);
  const omitted = result.candidates.length - shown.length;
  const list = shown.map(formatCandidate).join("\n");
  const moreLine = omitted > 0 ? `\n…${omitted} more — full list in details.candidates` : "";
  // P4a: the ordinal was offered and deliberately NOT honoured. This is the
  // django__django-13128 refusal -- ordinal:1 there indexed
  // ResolvedOuterRef.resolve_expression and overwrote it with
  // CombinedExpression logic, silently. See parentNameWouldDisambiguate.
  if (result.ordinalRefused === true) {
    return {
      isError: true,
      text:
        `weave_edit: ordinal:${params.entity.ordinal} refused for "${params.entity.name}" in ${params.file} — these ${result.candidates.length} same-named entities each sit under a DIFFERENT parent, ` +
        `and an ordinal indexes a start-line-sorted list that silently re-points at another entity when the file changes. Pass parent_name instead:\n${list}${moreLine}`,
      details: {
        file: params.file,
        entity: params.entity,
        resolved: false,
        ordinal_refused: true,
        candidates: result.candidates.map(candidateSummary),
        coordination,
        merge,
      },
    };
  }
  return {
    isError: true,
    text: `weave_edit: "${params.entity.name}" is ambiguous in ${params.file} — ${result.candidates.length} matches. Add entity_type, parent_name, or ordinal to disambiguate:\n${list}${moreLine}`,
    details: {
      file: params.file,
      entity: params.entity,
      resolved: false,
      candidates: result.candidates.map((e) => ({
        name: e.name,
        type: e.type,
        parent_name: e.parentName,
        start_line: e.start_line,
        end_line: e.end_line,
      })),
      coordination,
      merge,
    },
  };
}

function formatVerificationFailure(params: OneWeaveEditParams, entity: Entity, reason: string, coordination: CoordinationStatus, merge: MergeStatus): WeaveEditOutcome {
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""}, lines ${entity.start_line}-${entity.end_line})`;
  return {
    isError: true,
    text: `weave_edit: ${params.op} on ${location} in ${params.file} failed verification and was rolled back. ${reason}`,
    details: {
      file: params.file,
      op: params.op,
      entity: { name: entity.name, type: entity.type, parent_name: entity.parentName, start_line: entity.start_line, end_line: entity.end_line },
      verification: { ok: false, reason },
      rolledBack: true,
      coordination,
      merge,
    },
  };
}

function describeIdentityChange(c: IdentityChange): string {
  if (c.field === "visibility") return `was ${c.before}, now ${c.after}`;
  return `${c.field} changed from "${c.before}" to "${c.after}"`;
}

function formatIdentityChangeFailure(
  params: OneWeaveEditParams,
  entity: Entity,
  changes: IdentityChange[],
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""}, lines ${entity.start_line}-${entity.end_line})`;
  const summary = changes.map(describeIdentityChange).join("; ");
  return {
    isError: true,
    text: `weave_edit: replace on ${location} in ${params.file} changes its ${changes.map((c) => c.field).join("/")} — ${summary}. Refused and rolled back. Pass allow_signature_change: true to do this on purpose.`,
    details: {
      file: params.file,
      op: params.op,
      entity: { name: entity.name, type: entity.type, parent_name: entity.parentName, start_line: entity.start_line, end_line: entity.end_line },
      identityChange: { ok: false, changes },
      rolledBack: true,
      coordination,
      merge,
    },
  };
}

/** Names are inlined into result text only up to here; bigger lists stay in details. */
const MAX_INLINE_DEPENDENT_NAMES = 3;

function formatDependentsText(dependents: DependentsReport, op: OneWeaveEditParams["op"]): string {
  if (!dependents.checked) return dependents.reason ? ` Dependents check skipped: ${dependents.reason}.` : "";

  const before = dependents.before ?? [];
  if (before.length === 0) return " No other entities reference this one (per sem's dependency graph).";

  // A long inline name list is exactly the kind of fat result text the eval
  // flagged, so past a few dependents the text states only the count and
  // points at details.dependents.before (which always carries the full list).
  const describe = (d: DependentInfo) => `${d.name} (${d.file})`;
  const names = before.map(describe).join(", ");
  const plural = (n: number) => `entit${n === 1 ? "y" : "ies"}`;

  if (op === "delete") {
    if (before.length > MAX_INLINE_DEPENDENT_NAMES) {
      return ` ${before.length} entities referenced this before deletion — those references are now dangling. Full list: details.dependents.before.`;
    }
    return ` ${before.length} ${plural(before.length)} referenced this before deletion: ${names} — those references are now dangling.`;
  }

  if (dependents.afterCheckFailed) {
    if (before.length > MAX_INLINE_DEPENDENT_NAMES) {
      return ` ${before.length} entities referenced this before the edit. The after-edit check failed (${dependents.reason ?? "unknown reason"}), so whether they still resolve is unconfirmed. Full list: details.dependents.before.`;
    }
    return ` ${before.length} entit${before.length === 1 ? "y" : "ies"} referenced this before the edit: ${names}. The after-edit check failed (${dependents.reason ?? "unknown reason"}), so whether they still resolve is unconfirmed.`;
  }

  const after = dependents.after ?? [];
  const afterKeys = new Set(after.map((d) => `${d.file}::${d.name}`));
  const missing = before.filter((d) => !afterKeys.has(`${d.file}::${d.name}`));

  if (missing.length > 0) {
    if (missing.length > MAX_INLINE_DEPENDENT_NAMES) {
      return ` ${missing.length} of ${before.length} referencing entities no longer show up as a dependent after this edit — check them. Full list: details.dependents (before/after).`;
    }
    return ` ${missing.length} of ${before.length} referencing entit${before.length === 1 ? "y" : "ies"} no longer show up as a dependent after this edit: ${missing.map(describe).join(", ")} — check them.`;
  }

  if (before.length > MAX_INLINE_DEPENDENT_NAMES) {
    return ` ${before.length} entities reference this (per sem's syntax-level graph — not a compile check). Full list: details.dependents.before.`;
  }
  return ` ${before.length} ${plural(before.length)} reference this: ${names}. sem's check is syntax-level and doesn't model export/visibility, so this only confirms they still parse as a reference — it isn't a compile check.`;
}

function formatCoordinationText(coordination: CoordinationStatus): string {
  if (!coordination.attempted) return `weave-mcp coordination skipped: ${coordination.skippedReason ?? "claim=false"}`;
  if (!coordination.claimed) return `weave-mcp coordination skipped: ${coordination.skippedReason ?? "unknown reason"}`;
  if (coordination.released) return "coordinated via weave-mcp (claimed, updated, released)";
  return `claimed via weave-mcp but release failed: ${coordination.releaseError ?? "unknown error"}`;
}

function formatMergeConflictFailure(
  params: OneWeaveEditParams,
  entity: Entity,
  conflicts: MergeConflictSummary[],
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""})`;
  const names = conflicts.map((c) => c.entity_name ?? "?").join(", ");
  return {
    isError: true,
    text: `weave_edit: ${params.op} on ${location} in ${params.file} refused -- the file changed since it was read, and the concurrent change collides on the same entit${conflicts.length === 1 ? "y" : "ies"} (${names}). Nothing was written. Re-read the file and re-apply your edit against the current content.`,
    details: {
      file: params.file,
      op: params.op,
      entity: { name: entity.name, type: entity.type, parent_name: entity.parentName, start_line: entity.start_line, end_line: entity.end_line },
      mergeConflicts: conflicts,
      rolledBack: false,
      coordination,
      merge,
    },
  };
}

function formatMergeDroppedOursFailure(
  params: OneWeaveEditParams,
  entity: Entity,
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""})`;
  return {
    isError: true,
    text: `weave_edit: ${params.op} on ${location} in ${params.file} refused -- the merge backstop reconciled the concurrent change but its output no longer contains this edit's own text, so writing it would silently drop your edit. Nothing was written to disk. Re-read the file and re-apply against the current content.`,
    details: {
      file: params.file,
      op: params.op,
      entity: { name: entity.name, type: entity.type, parent_name: entity.parentName, start_line: entity.start_line, end_line: entity.end_line },
      mergeGuard: { ok: false, reason: "merged output dropped this edit's text" },
      rolledBack: false,
      coordination,
      merge,
    },
  };
}

/**
 * Which of the forward commit's three guard points lost its frame. All three
 * mean the same thing to a caller -- this edit's bytes are not on disk and
 * the file is hot -- so they share one summand and one details shape; the
 * phase only sharpens the sentence.
 */
type WriteWindowPhase = "gate-retry" | "commit" | "confirmation";

const WRITE_WINDOW_REASON: Record<WriteWindowPhase, string> = {
  "gate-retry": "a concurrent writer landed inside the merge gate's pre-write window on every attempt",
  commit: "a concurrent writer changed the file between this edit's read and its write, so the write was refused rather than clobbering it",
  confirmation: "a concurrent writer overwrote this edit's bytes immediately after they were written",
};

/**
 * The write-window refusal: a concurrent writer held the file at every point
 * this edit could have safely committed, so nothing of this edit is on disk.
 * That is the point: the alternative is writing a verdict known to be wrong,
 * or claiming a success whose bytes a stranger already replaced.
 *
 * "gate-retry" is the bounded-retry exhaustion (every merge verdict was
 * already stale by the time it could be acted on). "commit" is the
 * uncoordinated path's guarded write losing its frame before writing.
 * "confirmation" is the post-write read-back finding someone else's bytes.
 */
function formatWriteWindowLostFailure(
  params: OneWeaveEditParams,
  entity: Entity,
  attempts: number,
  phase: WriteWindowPhase,
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""})`;
  const cause =
    phase === "gate-retry"
      ? `a concurrent writer outran ${attempts} merge attempts`
      : phase === "commit"
        ? `a concurrent writer changed ${params.file} between this edit's read and its write, so nothing was overwritten`
        : `a concurrent writer replaced this edit's bytes the moment they landed`;
  return {
    isError: true,
    text: `weave_edit: ${params.op} on ${location} in ${params.file} refused -- ${cause}; ${params.file} is hot right now. Nothing of this edit is on disk. Retry, or coordinate with the other agent editing this file.`,
    details: {
      file: params.file,
      op: params.op,
      entity: { name: entity.name, type: entity.type, parent_name: entity.parentName, start_line: entity.start_line, end_line: entity.end_line },
      writeWindow: { ok: false, attempts, phase, reason: WRITE_WINDOW_REASON[phase] },
      rolledBack: false,
      coordination,
      merge,
    },
  };
}

/**
 * The guarded compensation's refusal, and the summand this engine gained
 * when the abort path was symmetrized with the commit path.
 *
 * The edit failed (verification, or the identity refusal), so a rollback to
 * the pre-edit snapshot was due -- but guardedWrite found bytes on disk that
 * this edit did not write. Some other process's write landed between this
 * edit's commit and its abort. Restoring the snapshot would have destroyed
 * that writer's work with content older than its own read: silent loss for a
 * bystander, the exact defect this summand exists to refuse.
 *
 * So nothing is restored, and the receipt says so. What the caller learns:
 * the edit did not stand, the rollback did not run, nothing was destroyed,
 * and the file now holds a third party's content -- which is why re-reading
 * before retrying is the only safe move. The images are reported by size and
 * hash so "these are different files" is citable without inlining bytes.
 */
function formatRollbackWindowLostFailure(
  params: OneWeaveEditParams,
  o: Extract<QueueOutcome, { kind: "rollback-window-lost" }>,
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  const entity = o.entity;
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""}, lines ${entity.start_line}-${entity.end_line})`;
  const why =
    o.cause === "verification-failed"
      ? `failed verification (${o.verificationReason ?? "verification failed"})`
      : `changes its ${(o.changes ?? []).map((c) => c.field).join("/")} and was refused`;
  return {
    isError: true,
    text:
      `weave_edit: ${params.op} on ${location} in ${params.file} ${why}, but the rollback was SKIPPED -- ${params.file} changed underneath this edit between its write and the rollback, ` +
      `so restoring the pre-edit snapshot would have destroyed another writer's bytes. Nothing was restored and nothing was destroyed; ${params.file} now holds content this edit did not write. Re-read it before retrying.`,
    details: {
      file: params.file,
      op: params.op,
      entity: { name: entity.name, type: entity.type, parent_name: entity.parentName, start_line: entity.start_line, end_line: entity.end_line },
      ...(o.cause === "verification-failed"
        ? { verification: { ok: false, reason: o.verificationReason ?? "verification failed" } }
        : { identityChange: { ok: false, changes: o.changes ?? [] } }),
      rolledBack: false,
      rollbackWindow: {
        ok: false,
        cause: o.cause,
        reason: "the file changed underneath this edit between its write and the rollback; the pre-edit snapshot was NOT restored over the other writer's bytes",
        expected: o.expected,
        actual: o.actual,
      },
      coordination,
      merge,
    },
  };
}

/** The one-line honesty note for a performed merge: names what changed underneath, capped like dependents. */
function formatMergeText(merge: MergeStatus): string {
  if (!merge.performed) return "";
  const over = merge.mergedOver ?? [];
  const shown = over.slice(0, MAX_INLINE_DEPENDENT_NAMES);
  const suffix = over.length > shown.length ? ` (+${over.length - shown.length} more, see details.merge.mergedOver)` : "";
  return ` Merged over concurrent changes to ${shown.join(", ") || "other entities"}${suffix}.`;
}

function formatSuccess(
  params: OneWeaveEditParams,
  entity: Entity,
  newRange: { start: number; end: number } | undefined,
  dependents: DependentsReport,
  coordination: CoordinationStatus,
  merge: MergeStatus,
): WeaveEditOutcome {
  const location = `${entity.name} (${entity.type}${entity.parentName ? ` in ${entity.parentName}` : ""})`;
  const oldRange = `${entity.start_line}-${entity.end_line}`;
  const rangeText = params.op === "delete" ? `deleted, was lines ${oldRange}` : `lines ${oldRange} -> ${newRange ? `${newRange.start}-${newRange.end}` : "?"}`;

  return {
    isError: false,
    text: `weave_edit: ${params.op} ${location} in ${params.file}, ${rangeText}. Verification: ok.${formatMergeText(merge)}${formatDependentsText(dependents, params.op)} ${formatCoordinationText(coordination)}.`,
    details: {
      file: params.file,
      op: params.op,
      entity: {
        name: entity.name,
        type: entity.type,
        parent_name: entity.parentName,
        old_start_line: entity.start_line,
        old_end_line: entity.end_line,
        // The splice is always line-based (see internal/text.ts); byte ranges
        // from sem are reported only as a reliability signal, never used to cut.
        range_source: "line",
        byte_range_reliable: entity.byteRangeReliable,
      },
      new_range: newRange ? { start_line: newRange.start, end_line: newRange.end } : null,
      verification: { ok: true },
      dependents,
      coordination,
      merge,
    },
  };
}

function lineAt(content: string, lineNumber: number): string {
  return content.split(/\r\n|\n/)[lineNumber - 1] ?? "";
}

/**
 * Locates the entity a replace's new content produced, WITHIN the spliced
 * range rather than at its exact first line. A leading doc comment/JSDoc
 * block shifts sem's own reported start_line past it (confirmed
 * empirically: a bare Rust `pub fn tokenize` has start_line 1; add a
 * `/// ...` line above it and sem reports start_line 2 for the SAME
 * function — TS JSDoc `/** *\/` does the same; Python decorators do NOT,
 * sem keeps those inside the entity's own start_line already). An exact
 * `start_line === range.start` match therefore misses the entity entirely
 * whenever the new content leads with such a comment, reporting a false
 * "no entity found at the edited location" identity change.
 *
 * Prefers an identity match (name+type+parent unchanged) anywhere in the
 * range — this is the case that matters most: a same-signature edit with
 * only a leading comment added must read as "unchanged", not as an
 * unrelated pick. Falls back to the "root" entity of the range (not nested
 * inside another entity that's also in range) when no identity match
 * exists, so a genuine rename/kind/parent change is still reported
 * accurately rather than silently waved through.
 */
function findAfterEntity(entitiesAfter: Entity[], before: Entity, range: { start: number; end: number }): Entity | undefined {
  const inRange = entitiesAfter.filter((e) => e.start_line >= range.start && e.start_line <= range.end);
  if (inRange.length === 0) return undefined;

  const identityMatch = inRange.find((e) => e.name === before.name && e.type === before.type && e.parentName === before.parentName);
  if (identityMatch) return identityMatch;

  const rootCandidates = inRange.filter((e) => e.parentName === null || !inRange.some((p) => p.name === e.parentName));
  const pool = rootCandidates.length > 0 ? rootCandidates : inRange;
  return pool.reduce((topmost, e) => (e.start_line < topmost.start_line ? e : topmost));
}

/**
 * How many times one edit will re-run the merge gate against a freshly
 * re-read disk before giving up. Each extra attempt costs one readFile plus
 * one weave_update_entity_content round trip, and only happens when a
 * concurrent writer actually landed inside this edit's gate->write window.
 * Five is enough that a losing writer has to be re-outrun five times in a
 * row to produce a refusal; past that the file is genuinely hot and saying
 * so beats looping.
 */
const MAX_MERGE_ATTEMPTS = 5;

/**
 * The edit engine's receipt coproduct, in its internal form: eight
 * constructors, every assignment site immediately followed by `return`, and
 * a total case analysis at the tail of performOneWeaveEdit mapping each to
 * exactly one formatter. `rollback-window-lost` is the summand the symmetric
 * transaction added -- the abort path can now refuse, the same way the
 * commit path always could. Witnessed in test/laws/no-silent-loss.law.test.ts
 * (enumeration totality) and test/laws/rollback-guardedness.law.test.ts (the
 * new summand's driver).
 */
type QueueOutcome =
  | { kind: "resolution-failed"; result: Extract<ResolveResult, { kind: "not-found" | "ambiguous" }> }
  | { kind: "verification-failed"; entity: Entity; reason: string }
  | { kind: "identity-changed"; entity: Entity; changes: IdentityChange[] }
  | { kind: "merge-conflict"; entity: Entity; conflicts: MergeConflictSummary[] }
  | { kind: "merge-dropped-ours"; entity: Entity }
  | { kind: "write-window-lost"; entity: Entity; attempts: number; phase: WriteWindowPhase }
  | {
      kind: "rollback-window-lost";
      entity: Entity;
      cause: "verification-failed" | "identity-changed";
      /** Set when cause is "verification-failed" -- why the edit was being aborted. */
      verificationReason?: string;
      /** Set when cause is "identity-changed" -- which identity fields moved. */
      changes?: IdentityChange[];
      /** The image the rollback expected to overwrite (this edit's own last write) vs the one it actually found. */
      expected: ImageSummary;
      actual: ImageSummary;
    }
  | {
      kind: "success";
      entity: Entity;
      newRange: { start: number; end: number } | undefined;
      insertedText: string | undefined;
      dependents: DependentsReport;
      /** The entity's post-edit name, if a replace renamed it (allow_signature_change) — equal to entity.name otherwise. */
      afterEntityName: string;
    };

/**
 * One entity's full edit orchestration — the engine performWeaveEdit
 * dispatches to for both its single-entity form and each item of an
 * edits= batch. Order: validate params, claim (best-effort, advisory only
 * — it protects nothing) before touching disk, splice inside the
 * file-mutation queue (re-reading the file there in case another tool
 * changed it first), run the merge gate, write only against a disk that
 * has not moved since the gate saw it and confirm the edit landed —
 * retrying the gate up to MAX_MERGE_ATTEMPTS times if it did — verify by
 * re-extracting entities and rolling back on failure, then release the
 * claim regardless of outcome.
 */
async function performOneWeaveEdit(params: OneWeaveEditParams, deps: WeaveEditDeps): Promise<WeaveEditOutcome> {
  const { cwd, semBin, coordinator, signal } = deps;
  const claimRequested = params.claim ?? true;

  if (params.op !== "delete" && params.content === undefined) {
    return {
      isError: true,
      text: `weave_edit: "content" is required for op "${params.op}".`,
      details: { file: params.file, op: params.op, entity: params.entity },
    };
  }

  const absPath = resolveTargetPath(cwd, params.file);

  let coordination: CoordinationStatus = { attempted: false, claimed: false };
  let repoLoc: RepoLocation | undefined;
  let branch = "HEAD";
  // the claim's own stable identity, when the connected
  // weave-mcp returns one — threaded to updateAndRelease below so a rename
  // between claim and release can't re-key this claim out from under us.
  // Older servers never populate this, and coordinator.claim only ever
  // returns an id it read back from THIS server's own response, so passing
  // it back to the same server later is always safe (see
  // weave-coordination.ts's queryArgs/parseEntityId doc comments).
  let claimedEntityId: string | undefined;

  if (!claimRequested) {
    coordination = { attempted: false, claimed: false, skippedReason: "claim=false" };
  } else if (!coordinator) {
    coordination = { attempted: true, claimed: false, skippedReason: "weave-mcp coordination is not configured for this tool" };
  } else {
    repoLoc = await repoRelativePath(absPath);
    if (!repoLoc) {
      coordination = { attempted: true, claimed: false, skippedReason: "file is not inside a git repository weave-mcp can address" };
    } else {
      branch = await currentBranch(repoLoc.root);
      const claimResult = await coordinator.claim(repoLoc.relPath, branch, params.entity, signal);
      // A LOST claim (another agent holds this entity) does NOT stop the edit
      // today — advisory-only is the considered choice, not silence: claims
      // are a cheap contention hint, and the correctness story rests on the
      // merge backstop that runs regardless of claim outcome. What this
      // branch guarantees is that the caller-visible status TELLS THE TRUTH:
      // claimed:false with the actual holder named in skippedReason, never a
      // misreported claimed:true for a claim this agent never won.
      coordination = claimResult.ok
        ? { attempted: true, claimed: true }
        : { attempted: true, claimed: false, skippedReason: claimResult.reason };
      if (claimResult.ok) claimedEntityId = claimResult.entityId;
    }
  }

  const allowSignatureChange = params.allow_signature_change ?? false;
  let queueOutcome!: QueueOutcome;
  let queueThrew = false;
  let queueError: unknown;
  // Explicit uncoordinated default -- see MergeStatus's doc comment.
  let merge: MergeStatus = { attempted: false, performed: false };
  // The gate's non-conflict paths store the entity's NEW content in the
  // CRDT before the local write. Two consequences tracked here: the legacy
  // post-edit content push becomes redundant (crdtUpdatedByGate), and a
  // rollback after the gate leaves the CRDT ahead of disk, which must be
  // resynced with the restored entity text (resyncEntityText).
  let crdtUpdatedByGate = false;
  let resyncEntityText: string | undefined;

  // A throw inside the callback (file deleted between claim and read, sem
  // erroring, an abort mid-call, ...) must not skip the claim release below —
  // weave-mcp has no independent claim timeout, so a skipped release orphans
  // the claim and blocks every other agent from claiming that entity. Capture
  // the throw here without swallowing it; the original error is re-thrown
  // after the release logic has run.
  try {
    await withFileMutationQueue(absPath, async () => {
      const currentContent = await readFile(absPath, "utf8");
      const entitiesBefore = await extractEntities(semBin, absPath, cwd, signal);
      const resolved = resolveEntity(entitiesBefore, params.entity);

      if (resolved.kind !== "found") {
        queueOutcome = { kind: "resolution-failed", result: resolved };
        return;
      }

      // Dependents are checked against the entity's state before any write —
      // deleting it makes it unresolvable, so "who depended on this" has to
      // be captured now or not at all.
      const dependentsBefore =
        params.op === "replace" || params.op === "delete"
          ? await checkDependents(semBin, cwd, absPath, resolved.entity.name, signal, ownEntityId(resolved.entity, cwd, absPath))
          : undefined;

      const spliced = splice(currentContent, resolved.entity, params.op as Op, params.content);

      // Merge backstop -- the UNCONDITIONAL pre-write gate whenever
      // coordination is reachable, deliberately NOT gated on the claim
      // outcome: claims are visibility only (a contention hint other agents
      // can read, measured at ~3-4ms per edit and zero protection -- a lost
      // claim does not stop this edit, and claims are per-ENTITY, so two
      // agents in different functions of one file never contend), and the
      // correctness story rests here. One weave_update_entity_content call
      // carries the whole-file snapshot pair (base_content = the file as
      // read above, ours_content = the file as spliced) and doubles as the
      // CRDT content update on every non-conflict path. claim=false is the
      // caller's explicit full opt-out of coordination and skips this too.
      let textToWrite = spliced.text;
      let newRange = spliced.newRange;
      let written = false;
      // Entities extracted FROM textToWrite by the commit path (its
      // deliberate delay before the post-write read-back), handed to
      // verification below so the extraction is not paid twice.
      let entitiesOfWrittenText: Entity[] | undefined;

      // "Did this edit actually land?", asked of the bytes on disk -- the
      // one post-write question that is genuinely ABOUT disk and so is
      // still answered by reading it. For a delete the edit IS the absence
      // of the old source; for everything else it is the presence of the
      // spliced-in text, the same whole-file search the merge identity
      // guard below uses.
      const removedText = params.op === "delete" ? sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line) : undefined;
      const landedOn = (disk: string): boolean =>
        removedText !== undefined ? !disk.includes(removedText) : spliced.insertedText === undefined || disk.includes(spliced.insertedText);

      if (claimRequested && coordinator && repoLoc) {
        // The snapshot PAIR sent to the gate never changes across retries:
        // base_content is the file as this edit read it and ours_content is
        // that same file with this edit's splice -- their diff IS this
        // edit, and re-basing either one onto a concurrent writer's bytes
        // would silently subtract that writer's change from the diff. What
        // a retry refreshes is the SERVER's side: it re-reads disk and
        // re-merges base->ours against whatever landed there since.
        let attempts = 0;

        for (;;) {
          attempts++;
          // Disk as this process sees it immediately BEFORE the gate call.
          // The verdict is trustworthy exactly when the disk is unchanged
          // from this across the round trip -- see the re-check below.
          const observed = await readFile(absPath, "utf8");
          const gate = await coordinator.mergeCheck(
            repoLoc.relPath,
            branch,
            params.entity,
            params.op === "delete" ? "" : (spliced.insertedText ?? ""),
            currentContent,
            spliced.text,
            signal,
            claimedEntityId,
          );
          if (!gate.ok && gate.conflict) {
            // A true same-entity collision: nothing was written anywhere --
            // not by the server (it refused before storing) and not here.
            merge = { attempted: true, performed: false, driftDetected: true };
            queueOutcome = { kind: "merge-conflict", entity: resolved.entity, conflicts: gate.conflicts };
            return;
          }
          if (gate.ok) {
            crdtUpdatedByGate = true;
            // The server's merged_over audit lists every entity whose
            // resolution was not "unchanged" -- including the one THIS edit
            // changed. "Merged over concurrent changes" means what changed
            // underneath, so this edit's own entity is filtered out.
            const mergedOverOthers = (gate.mergedOver ?? []).filter((n) => n !== params.entity.name);
            merge = {
              attempted: true,
              performed: gate.merged,
              ...(gate.drift !== undefined ? { driftDetected: gate.drift } : {}),
              ...(gate.merged ? { mergedOver: mergedOverOthers } : {}),
            };
            // A retry recomputes the WHOLE verdict from the unchanged
            // snapshot pair, so the latest one is always the complete one
            // -- textToWrite is reset from the splice first so a merged
            // result from an earlier attempt can never leak into a later
            // "no drift" verdict that did not produce one.
            textToWrite = spliced.text;
            newRange = spliced.newRange;
            if (gate.merged && gate.mergedContent !== undefined) {
              // Identity guard: the merge engine's output must still carry
              // this edit's own text, whole -- a whole-file search, not a
              // trust-the-flag check. A merge that dropped ours is refused
              // (and the CRDT, which stored ours, is resynced below).
              if (params.op !== "delete" && spliced.insertedText !== undefined && !gate.mergedContent.includes(spliced.insertedText)) {
                merge = { attempted: true, performed: false, driftDetected: true };
                resyncEntityText = sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line);
                queueOutcome = { kind: "merge-dropped-ours", entity: resolved.entity };
                return;
              }
              textToWrite = gate.mergedContent;
              // The splice's line range is stale once concurrent changes
              // above this entity are merged in -- recompute from where the
              // inserted text actually landed in the merged file.
              if (spliced.insertedText !== undefined) {
                const at = textToWrite.indexOf(spliced.insertedText);
                if (at >= 0) {
                  const start = textToWrite.slice(0, at).split(/\r\n|\n/).length;
                  const end = start + spliced.insertedText.split(/\r\n|\n/).length - 1;
                  newRange = { start, end };
                }
              }
            }
          } else {
            // Transport-level failure: the gate could not run. Advisory like
            // the claim itself -- the edit proceeds, the status tells the
            // truth, and the legacy post-edit content push below still runs.
            // Retrying is pointless (there is no verdict to refresh and the
            // next call would fail the same way), so this leaves the loop
            // with the pre-fix TOCTOU exposure it always had.
            merge = { attempted: true, performed: false };
            break;
          }

          // Pre-write freshness re-check -- the close on the gate's
          // check-then-write TOCTOU. The verdict just computed describes the
          // disk as the SERVER read it, one MCP round trip ago; another OS
          // process can land its own write in the gap between that RPC
          // returning and the guardedWrite below, and that write is
          // invisible to the verdict this code is about to act on. That is
          // precisely what the guard's expected image catches. Measured on this
          // engine before this loop existed: two free-running processes
          // editing DISJOINT entities in one file silently lost at least one
          // edit in 5/5 runs (11 of 200 edits gone, no error surfaced).
          //
          // So: bracket the gate call with two reads of the same file and
          // compare them. Equal -> nothing landed while the RPC was in
          // flight, so the disk the server merged against is the disk this
          // write lands on, and the verdict is current -- write it, in the
          // SAME synchronous step as the comparison. Unequal -> a writer
          // landed inside the window and the verdict describes a file that
          // no longer exists; re-run the SAME gate with the SAME snapshot
          // pair so the server re-merges base->ours against the new disk.
          // Reconciliation stays the server's three-way merge, never a local
          // guess.
          //
          // The compare-adjacent-write itself, and the honest residual it
          // carries, now live in guardedWrite -- this loop's only remaining
          // job is choosing the expected image (`observed`) and deciding
          // what a lost frame means here (re-run the gate).
          if (guardedWrite(absPath, observed, textToWrite).kind === "written") {
            // Post-write confirmation -- the half that makes the loop
            // SOUND rather than merely narrow, and it is not optional. The
            // guarded write above is optimistic: it prevents most clobbers
            // but cannot prevent the one where the other process is inside
            // its own identical window at the identical instant. Measured:
            // with only the compare-and-write, two agents spawned together
            // are phase-locked tightly enough to collide on their very
            // first edit, and 3 of 5 runs still lost one.
            //
            // So instead of pretending the write is a compare-and-swap,
            // this asks the only question that actually settles it, and
            // asks it LATE: entities are extracted first (a sem subprocess
            // -- tens of ms, far longer than any racing writer's remaining
            // window), and only then is the file read back and checked for
            // this edit's own text. Present -> the edit landed and stands.
            // Absent -> a concurrent write overwrote it, which the pre-write
            // compare could not see; go round again and let the gate
            // re-merge against the new disk. Whoever loses the race is the
            // one who detects it, so an edit silently vanishing now requires
            // the loser's whole extraction to finish inside the winner's
            // write window.
            //
            // The extraction is of textToWrite, NOT of the file: it is the
            // input to verification below, which is a question about this
            // transaction's own output (see extractEntitiesFromText). It
            // still costs a sem subprocess, so it still serves as the delay;
            // it just can no longer be torn by a foreign writer. The read
            // back on the next line is the disk observation, and the only
            // one this path makes.
            entitiesOfWrittenText = await extractEntitiesFromText(semBin, textToWrite, absPath, cwd, signal);
            if (landedOn(await readFile(absPath, "utf8"))) {
              written = true;
              break;
            }
          }
          if (attempts >= MAX_MERGE_ATTEMPTS) {
            // Refuse rather than write a verdict known to be stale. The CRDT
            // holds this edit's entity text from the gate above but disk
            // never got it, so resync it the same way the other
            // written-nowhere paths do.
            if (crdtUpdatedByGate) resyncEntityText = sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line);
            queueOutcome = { kind: "write-window-lost", entity: resolved.entity, attempts, phase: "gate-retry" };
            return;
          }
        }
      }

      // The other forward-commit site: the paths that never reach the gate
      // loop -- coordination not reachable (claim=false, no weave-mcp, not a
      // git repo) and the gate's own transport failure. There is no drift
      // verdict to keep fresh here, but the obligation not to destroy a
      // stranger's bytes is the same, so it goes through the SAME primitive,
      // with the file as this edit read it as the expected image. Losing
      // that guard means an outside process wrote between this edit's read
      // and its write: refuse, rather than clobber it. (In-process,
      // withFileMutationQueue makes the guard a formality -- nothing else in
      // this process could have touched the file. Only another OS process
      // reaches it.)
      let confirmLanding = false;
      if (!written) {
        const commit = guardedWrite(absPath, currentContent, textToWrite);
        if (commit.kind === "lost") {
          if (crdtUpdatedByGate) resyncEntityText = sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line);
          queueOutcome = { kind: "write-window-lost", entity: resolved.entity, attempts: 1, phase: "commit" };
          return;
        }
        written = true;
        // No gate loop ran, so this path owes its own post-write
        // confirmation. It is deferred to AFTER verification below, for the
        // same reason the loop's is deferred behind an extraction: the later
        // the read-back, the smaller the window a racing writer has left to
        // finish inside.
        confirmLanding = true;
      }

      // VERIFICATION IS PURE. It asks whether the text THIS TRANSACTION
      // produced still parses cleanly around the change -- a question about
      // textToWrite, which is in memory, not about disk. Answering it with a
      // disk re-read made it answerable by a stranger: a foreign process
      // writing while `sem entities` read produced a torn parse, entities
      // vanished, and verifyEdit returned a false "untouched entities are
      // gone" verdict about a file state that never logically existed -- a
      // verdict that then drove a rollback over the top of that writer's
      // completed work. Extracting from the buffer deletes the failure mode
      // instead of defending against it. The commit path above already paid
      // for this extraction as its delay; reuse it.
      const entitiesAfter = entitiesOfWrittenText ?? (await extractEntitiesFromText(semBin, textToWrite, absPath, cwd, signal));
      const verification = verifyEdit(entitiesBefore, entitiesAfter, resolved.entity, params.op as Op);

      if (!verification.ok) {
        // The compensation is guarded exactly like the commit: restore only
        // over this edit's OWN last write. Foreign bytes mean the frame
        // moved, and the honest outcome is to report it, never to blind-fire
        // a snapshot older than the other writer's own read.
        const restore = guardedWrite(absPath, textToWrite, currentContent);
        if (crdtUpdatedByGate) resyncEntityText = sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line);
        if (restore.kind === "lost") {
          queueOutcome = {
            kind: "rollback-window-lost",
            entity: resolved.entity,
            cause: "verification-failed",
            verificationReason: verification.reason ?? "verification failed",
            expected: summarizeImage(textToWrite),
            actual: summarizeImage(restore.actualImage),
          };
          return;
        }
        queueOutcome = { kind: "verification-failed", entity: resolved.entity, reason: verification.reason ?? "verification failed" };
        return;
      }

      let afterEntityName = resolved.entity.name;
      // The post-edit twin of the pre-edit id above: re-derived from the
      // entity as re-extracted after the write (its parent/type may have
      // moved), so the "after" half of the dependents report is addressed
      // just as precisely as the "before" half.
      let afterEntityId = ownEntityId(resolved.entity, cwd, absPath);

      if (params.op === "replace") {
        const beforeFacts: IdentityFacts = {
          name: resolved.entity.name,
          type: resolved.entity.type,
          parentName: resolved.entity.parentName,
          visibility: deriveVisibility(absPath, resolved.entity.name, lineAt(currentContent, resolved.entity.start_line), currentContent),
        };
        const afterEntity = newRange ? findAfterEntity(entitiesAfter, resolved.entity, newRange) : undefined;
        const afterFacts: IdentityFacts | undefined = afterEntity
          ? {
              name: afterEntity.name,
              type: afterEntity.type,
              parentName: afterEntity.parentName,
              visibility: deriveVisibility(absPath, afterEntity.name, lineAt(textToWrite, afterEntity.start_line), textToWrite),
            }
          : undefined;

        const changes = compareIdentity(beforeFacts, afterFacts);
        if (changes.length > 0 && !allowSignatureChange) {
          // Same guard as the verification rollback -- an identity refusal is
          // the same compensating transaction, so it earns the same right to
          // overwrite: only this edit's own bytes.
          const restore = guardedWrite(absPath, textToWrite, currentContent);
          if (crdtUpdatedByGate) resyncEntityText = sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line);
          if (restore.kind === "lost") {
            queueOutcome = {
              kind: "rollback-window-lost",
              entity: resolved.entity,
              cause: "identity-changed",
              changes,
              expected: summarizeImage(textToWrite),
              actual: summarizeImage(restore.actualImage),
            };
            return;
          }
          queueOutcome = { kind: "identity-changed", entity: resolved.entity, changes };
          return;
        }

        // allow_signature_change may have let a rename through — look the new
        // name up under its post-edit identity, not the one it no longer has.
        if (afterEntity) {
          afterEntityName = afterEntity.name;
          afterEntityId = ownEntityId(afterEntity, cwd, absPath);
        }
      }

      // The uncoordinated commit's deferred post-write confirmation -- the
      // only disk observation this path makes after writing, and the reason
      // it can still say "verification: ok" honestly. Verification alone can
      // no longer notice a foreign overwrite (it never looks at disk), so
      // without this the receipt would claim a success whose bytes a
      // stranger had already replaced. Nothing to roll back if it fires:
      // this edit's write is already gone.
      if (confirmLanding && !landedOn(await readFile(absPath, "utf8"))) {
        if (crdtUpdatedByGate) resyncEntityText = sliceRestoredEntityText(currentContent, resolved.entity.start_line, resolved.entity.end_line);
        queueOutcome = { kind: "write-window-lost", entity: resolved.entity, attempts: 1, phase: "confirmation" };
        return;
      }

      let dependents: DependentsReport;
      if (!dependentsBefore) {
        dependents = { checked: false };
      } else if (!dependentsBefore.ok) {
        dependents = { checked: false, reason: dependentsBefore.reason };
      } else if (params.op === "delete") {
        dependents = { checked: true, before: dependentsBefore.dependents };
      } else {
        const dependentsAfter = await checkDependents(semBin, cwd, absPath, afterEntityName, signal, afterEntityId);
        dependents = dependentsAfter.ok
          ? { checked: true, before: dependentsBefore.dependents, after: dependentsAfter.dependents }
          : { checked: true, before: dependentsBefore.dependents, afterCheckFailed: true, reason: dependentsAfter.reason };
      }

      queueOutcome = {
        kind: "success",
        entity: resolved.entity,
        newRange,
        insertedText: spliced.insertedText,
        dependents,
        afterEntityName,
      };
    });
  } catch (err) {
    // The claim must still be released below even when the callback threw —
    // weave-mcp has no independent claim timeout, so skipping release here
    // orphans the claim and blocks every other agent from claiming this
    // entity. The original error is re-thrown after release runs, so
    // performWeaveEdit's caller-visible behavior on failure is unchanged.
    queueThrew = true;
    queueError = err;
  }

  // A rollback after the gate's CRDT update left the CRDT holding content
  // that never landed on disk -- push the restored entity text back,
  // regardless of claim outcome (the gate itself runs regardless of it).
  if (resyncEntityText !== undefined && coordinator && repoLoc) {
    await coordinator.update(repoLoc.relPath, branch, params.entity, resyncEntityText, signal, claimedEntityId);
  }

  if (coordination.claimed && coordinator && repoLoc) {
    // The gate already stored the entity's new content on every
    // non-conflict path -- the legacy post-edit push is only the fallback
    // for when the gate could not run (transport failure, or a server
    // predating the backstop would have errored the gate call).
    const newContent =
      !queueThrew && queueOutcome.kind === "success" && params.op !== "delete" && !crdtUpdatedByGate ? queueOutcome.insertedText : undefined;
    // A rename (allow_signature_change) can shift weave-mcp's own tracked
    // identity for this claim to the entity's NEW name as a side effect of
    // updateAndRelease's content update — releasing by the claim-time name
    // alone then fails server-side ("entity '<old name>' not found"),
    // orphaning the claim on an otherwise-successful edit. Pass the new
    // identity too so the coordinator can retry release under it.
    const renamedEntity =
      !queueThrew && queueOutcome.kind === "success" && queueOutcome.afterEntityName !== params.entity.name
        ? { ...params.entity, name: queueOutcome.afterEntityName }
        : undefined;
    const releaseResult = await coordinator.updateAndRelease(repoLoc.relPath, branch, params.entity, newContent, signal, renamedEntity, claimedEntityId);
    coordination = {
      ...coordination,
      updated: (newContent !== undefined && releaseResult.ok) || (crdtUpdatedByGate && queueOutcome?.kind === "success"),
      released: releaseResult.ok,
      ...(releaseResult.ok ? {} : { releaseError: releaseResult.reason }),
    };
  }

  if (queueThrew) throw queueError;

  if (queueOutcome.kind === "resolution-failed") return formatResolutionFailure(params, queueOutcome.result, coordination, merge);
  if (queueOutcome.kind === "verification-failed") return formatVerificationFailure(params, queueOutcome.entity, queueOutcome.reason, coordination, merge);
  if (queueOutcome.kind === "identity-changed") return formatIdentityChangeFailure(params, queueOutcome.entity, queueOutcome.changes, coordination, merge);
  if (queueOutcome.kind === "merge-conflict") return formatMergeConflictFailure(params, queueOutcome.entity, queueOutcome.conflicts, coordination, merge);
  if (queueOutcome.kind === "merge-dropped-ours") return formatMergeDroppedOursFailure(params, queueOutcome.entity, coordination, merge);
  if (queueOutcome.kind === "write-window-lost")
    return formatWriteWindowLostFailure(params, queueOutcome.entity, queueOutcome.attempts, queueOutcome.phase, coordination, merge);
  if (queueOutcome.kind === "rollback-window-lost") return formatRollbackWindowLostFailure(params, queueOutcome, coordination, merge);
  return formatSuccess(params, queueOutcome.entity, queueOutcome.newRange, queueOutcome.dependents, coordination, merge);
}

/** One resync attempt's outcome, reported per rolled-back-but-previously-succeeded edit. */
export interface CoordinationResyncNote {
  file: string;
  entity: EntityRef;
  resynced: boolean;
  reason?: string;
}

/** Same EOL-aware line slice sem_read's sliceEntitySource uses, kept local — this file already owns splice/parseLines/renderLines for its own edit path. */
function sliceRestoredEntityText(content: string, startLine: number, endLine: number): string {
  const model = parseLines(content);
  return renderLines({ eol: model.eol, hasTrailingNewline: false }, model.lines.slice(startLine - 1, endLine));
}

/**
 * performWeaveEditBatch's
 * atomic=true restores every touched FILE to its pre-batch snapshot when a
 * later edit fails, but any EARLIER edit in the same batch that had already
 * succeeded already ran its own full claim/update/release cycle against
 * weave-mcp (inside performOneWeaveEdit) before the failure was even hit —
 * its update pushed the (now-undone) post-edit content, and release already
 * let the claim go. The disk rollback above fixes the file; nothing fixed
 * weave-mcp's own tracked content for that entity, which still says the
 * (reverted) edit landed.
 *
 * For every edit before the failure point whose coordination was actually
 * claimed (skips edits run with claim=false or where coordination was
 * unavailable — nothing was ever pushed for those, so nothing to resync),
 * this re-extracts entities from the just-rolled-back file (not from the
 * pre-batch snapshot's line numbers directly — a second edit earlier in the
 * SAME batch touching the SAME file could have shifted line counts before
 * the failure, so the entity's true current position has to be re-resolved
 * from the restored file, not assumed stable) and re-runs the exact same
 * claim -> updateAndRelease cycle with the entity's real, restored content.
 *
 * Coordination stays best-effort even here (weave-coordination.ts's own
 * contract: a coordination failure must never block or fail the disk
 * operation, which has already completed by the time this runs) — a
 * re-claim that fails (e.g. another agent grabbed the entity in the gap
 * between the original release and this resync) is reported honestly as
 * unresynced rather than silently pretended away.
 */
async function resyncCoordinationAfterRollback(
  results: Array<{ file: string; entity: EntityRef; outcome: WeaveEditOutcome }>,
  stoppedAtIndex: number,
  deps: WeaveEditDeps,
): Promise<CoordinationResyncNote[]> {
  const { coordinator, cwd, semBin, signal } = deps;
  if (!coordinator) return [];

  const notes: CoordinationResyncNote[] = [];
  const restoredFileCache = new Map<string, string>();

  for (let i = 0; i < stoppedAtIndex; i++) {
    const r = results[i]!;
    if (r.outcome.isError) continue;

    const coordination = (r.outcome.details as { coordination?: CoordinationStatus }).coordination;
    if (!coordination?.claimed) continue;

    const absPath = resolveTargetPath(cwd, r.file);
    try {
      let restoredContent = restoredFileCache.get(r.file);
      if (restoredContent === undefined) {
        restoredContent = await readFile(absPath, "utf8");
        restoredFileCache.set(r.file, restoredContent);
      }

      const entitiesRestored = await extractEntities(semBin, absPath, cwd, signal);
      const resolved = resolveEntity(entitiesRestored, r.entity);
      if (resolved.kind !== "found") {
        notes.push({ file: r.file, entity: r.entity, resynced: false, reason: `entity no longer resolves in the rolled-back file (${resolved.kind})` });
        continue;
      }

      const restoredEntityText = sliceRestoredEntityText(restoredContent, resolved.entity.start_line, resolved.entity.end_line);

      const repoLoc = await repoRelativePath(absPath);
      if (!repoLoc) {
        notes.push({ file: r.file, entity: r.entity, resynced: false, reason: "file is not inside a git repository weave-mcp can address" });
        continue;
      }
      const branch = await currentBranch(repoLoc.root);

      const claimResult = await coordinator.claim(repoLoc.relPath, branch, r.entity, signal);
      if (!claimResult.ok) {
        notes.push({ file: r.file, entity: r.entity, resynced: false, reason: `could not re-claim to resync: ${claimResult.reason}` });
        continue;
      }
      const releaseResult = await coordinator.updateAndRelease(
        repoLoc.relPath,
        branch,
        r.entity,
        restoredEntityText,
        signal,
        undefined,
        claimResult.entityId,
      );
      notes.push({
        file: r.file,
        entity: r.entity,
        resynced: releaseResult.ok,
        reason: releaseResult.ok ? undefined : `re-claimed but failed to push restored content: ${releaseResult.reason}`,
      });
    } catch (err) {
      notes.push({ file: r.file, entity: r.entity, resynced: false, reason: err instanceof Error ? err.message : String(err) });
    }
  }
  return notes;
}

/**
 * Dogfood round 1, finding 4: a 9-site rename forced the model to
 * choose between 9 slow single-entity weave_edit calls or one unaudited
 * apply_patch bypass sweeping every remaining reference — it chose the
 * bypass for 8 of the 9 sites. edits= runs N entities (any mix of files)
 * in one call, EACH still going through performOneWeaveEdit's own full
 * claim/verify/dependents/release lifecycle (a "shared claim/release per
 * entity", not one shared claim across the whole batch) — so every site
 * gets the same safety net a single weave_edit call gets, just without one
 * tool round-trip per site.
 *
 * Non-atomic (default): edits run in order, each independently — one
 * edit's failure doesn't stop or undo the others (matching sem_read's
 * entities= and sem_find/sem_grep's batch convention elsewhere in this
 * package). atomic=true rolls back every edit in the batch the moment one
 * fails: every distinct file the batch touches is snapshotted before any
 * edit runs, and restored to that snapshot on failure — not just the
 * files touched by edits that already "succeeded", since two edits in the
 * same batch can touch the same file.
 */
async function performWeaveEditBatch(edits: OneWeaveEditParams[], atomic: boolean, deps: WeaveEditDeps): Promise<WeaveEditOutcome> {
  const distinctFiles = Array.from(new Set(edits.map((e) => e.file)));
  const snapshots = new Map<string, string>();
  if (atomic) {
    for (const file of distinctFiles) {
      try {
        snapshots.set(file, await readFile(resolveTargetPath(deps.cwd, file), "utf8"));
      } catch {
        // Missing/unreadable file: nothing to snapshot: an edit against it
        // will fail on its own and, if atomic, trigger rollback of the
        // OTHER files this batch did manage to touch.
      }
    }
  }

  const results: Array<{ file: string; entity: EntityRef; outcome: WeaveEditOutcome }> = [];
  let stoppedAtIndex = -1;
  // What THIS batch last left on each file. The atomic restore's guard needs
  // an expected image, and "the pre-batch snapshot" is the wrong one: by
  // rollback time the batch has written the file itself. Captured after each
  // edit so the guard compares against the batch's own most recent output.
  const batchImages = new Map<string, string>();

  for (let i = 0; i < edits.length; i++) {
    const edit = edits[i]!;
    const outcome = await performOneWeaveEdit(edit, deps);
    results.push({ file: edit.file, entity: edit.entity, outcome });
    if (atomic) {
      try {
        batchImages.set(edit.file, readFileSync(resolveTargetPath(deps.cwd, edit.file), "utf8"));
      } catch {
        // Unreadable/removed: the restore below falls back to the pre-batch
        // snapshot as its expected image and reports honestly if that fails.
      }
    }
    if (outcome.isError && atomic) {
      stoppedAtIndex = i;
      break;
    }
  }

  const rolledBack = atomic && stoppedAtIndex !== -1;
  let resyncNotes: CoordinationResyncNote[] = [];
  const restoreSkipped: Array<{ file: string; reason: string; expected?: ImageSummary; actual?: ImageSummary }> = [];
  if (rolledBack) {
    // The batch restore is a compensating transaction like any other, so it
    // goes through the same guard: undo only what this batch itself wrote. A
    // file an outside process touched since is left alone and named in the
    // receipt -- an all-or-nothing batch may not buy its atomicity with
    // somebody else's bytes.
    for (const [file, original] of snapshots) {
      const expected = batchImages.get(file) ?? original;
      try {
        const restore = guardedWrite(resolveTargetPath(deps.cwd, file), expected, original);
        if (restore.kind === "lost") {
          restoreSkipped.push({
            file,
            reason: "the file changed underneath this batch; the pre-batch snapshot was NOT restored over the other writer's bytes",
            expected: summarizeImage(expected),
            actual: summarizeImage(restore.actualImage),
          });
        }
      } catch (err) {
        restoreSkipped.push({ file, reason: `could not restore: ${err instanceof Error ? err.message : String(err)}` });
      }
    }
    // The disk is back to pre-batch state; any edit before the failure that
    // had already gone through its own claim/update/release cycle now has
    // stale content sitting in weave-mcp — resync it to match.
    resyncNotes = await resyncCoordinationAfterRollback(results, stoppedAtIndex, deps);
  }

  const succeeded = results.filter((r) => !r.outcome.isError).length;
  const attempted = results.length;
  const skipped = edits.length - attempted;

  const lines = results.map(({ file, entity, outcome }, i) => {
    const marker = outcome.isError ? "FAILED" : "ok";
    return `${i + 1}. [${marker}] ${file}#${entity.name}: ${outcome.text}`;
  });
  if (skipped > 0) lines.push(`…${skipped} not attempted (atomic batch stopped after edit ${stoppedAtIndex + 1} failed)`);

  if (restoreSkipped.length > 0) {
    lines.push(
      `rollback restore was SKIPPED for ${restoreSkipped.length} file(s) that changed underneath this batch: ${restoreSkipped
        .map((s) => `${s.file} (${s.reason})`)
        .join("; ")}. Nothing there was destroyed — re-read those files before retrying.`,
    );
  }

  const unresyncedNotes = resyncNotes.filter((n) => !n.resynced);
  if (unresyncedNotes.length > 0) {
    lines.push(
      `weave-mcp coordination could not be fully resynced after rollback for: ${unresyncedNotes
        .map((n) => `${n.file}#${n.entity.name} (${n.reason})`)
        .join("; ")}`,
    );
  }

  const header = rolledBack
    ? `weave_edit (batch, atomic): edit ${stoppedAtIndex + 1}/${edits.length} failed — all ${attempted} attempted edit(s) rolled back.`
    : `weave_edit (batch): ${succeeded}/${edits.length} edits applied.`;

  return {
    isError: rolledBack || succeeded === 0,
    text: [header, ...lines].join("\n"),
    details: {
      atomic,
      rolledBack,
      total: edits.length,
      succeeded,
      attempted,
      skipped,
      results: results.map((r) => ({ file: r.file, entity: r.entity, isError: r.outcome.isError, details: r.outcome.details })),
      ...(restoreSkipped.length > 0 ? { restoreSkipped } : {}),
      ...(resyncNotes.length > 0 ? { coordinationResync: resyncNotes } : {}),
    },
  };
}

/**
 * The tool's full public entry point. edits= (non-empty) dispatches to
 * performWeaveEditBatch; otherwise the single-entity form (file=/entity=/
 * op=) dispatches to performOneWeaveEdit directly, returning exactly what
 * it always has — no batch wrapper, so existing single-entity callers are
 * unaffected.
 */
export async function performWeaveEdit(params: WeaveEditParams, deps: WeaveEditDeps): Promise<WeaveEditOutcome> {
  const edits = params.edits;
  if (edits === undefined || edits.length === 0) {
    if (params.file === undefined || params.entity === undefined || params.op === undefined) {
      return {
        isError: true,
        text: "weave_edit: pass either { file, entity, op } or edits=[...].",
        details: { error: "missing file/entity/op" },
      };
    }
    return performOneWeaveEdit(
      { file: params.file, entity: params.entity, op: params.op, content: params.content, claim: params.claim, allow_signature_change: params.allow_signature_change },
      deps,
    );
  }

  const oneParams: OneWeaveEditParams[] = edits.map((e) => ({
    file: e.file,
    entity: e.entity,
    op: e.op,
    content: e.content,
    claim: params.claim,
    allow_signature_change: e.allow_signature_change,
  }));
  return performWeaveEditBatch(oneParams, params.atomic ?? false, deps);
}

export interface RegisterWeaveEditOptions {
  /** `sem` binary to shell out to for entity extraction. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
  /** weave-mcp binary for live coordination. Defaults to $PI_SEM_WEAVE_MCP_BIN, then "weave-mcp" (PATH). */
  weaveMcpCommand?: string;
  weaveMcpArgs?: string[];
  /** Stable identity for weave_agent_register/claim/release. Defaults to $PI_SEM_AGENT_ID, then a random per-process id. */
  agentId?: string;
  requestTimeoutMs?: number;
}

/** Registers the `weave_edit` tool. Call once per pi extension load. */
export function registerWeaveEdit(pi: ExtensionAPI, opts: RegisterWeaveEditOptions = {}): void {
  const semBin = opts.semBin ?? "sem";
  const weaveMcpCommand = opts.weaveMcpCommand ?? process.env.PI_SEM_WEAVE_MCP_BIN ?? "weave-mcp";
  const agentId = opts.agentId ?? process.env.PI_SEM_AGENT_ID ?? `pi-sem-${process.pid}-${Math.random().toString(36).slice(2, 8)}`;

  let coordinator: Coordinator | undefined;
  function getCoordinator(cwd: string): Coordinator {
    if (!coordinator) {
      coordinator = new Coordinator({ command: weaveMcpCommand, args: opts.weaveMcpArgs, cwd, requestTimeoutMs: opts.requestTimeoutMs, agentId });
    }
    return coordinator;
  }

  pi.registerTool({
    name: "weave_edit",
    label: "Weave Edit",
    description:
      "Edit one function/class/method (replace/insert/delete). Verifies the file still parses, rolling back if not. edits=[...] batches several.",
    promptSnippet: "Edit one named function/class/method in a file (replace/insert/delete) instead of a line-based edit",
    promptGuidelines: [
      'Use weave_edit instead of a line-based edit whenever the change is "replace this whole function/method/class" — give the FULL new source of the entity in `content`, including its signature and closing brace.',
      "Use weave_edit's entity.parent_name when the entity name exists in more than one class or module — omitting it when the name is ambiguous makes weave_edit refuse with the candidate list instead of guessing.",
      'Use weave_edit\'s op "insert_after"/"insert_before" to add a new function/method next to an existing one, and op "delete" to remove one, without hand-computing line ranges.',
      "Renaming or touching several entities at once (across one or many files)? Pass edits=[{file, entity, op, content?, allow_signature_change?}, ...] in one call instead of one weave_edit call per site — each still gets its own verify/dependents/coordination pass; add atomic:true only if the whole set must land together or not at all.",
    ],
    parameters: WeaveEditParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performWeaveEdit(params, {
        cwd: ctx.cwd,
        semBin,
        coordinator: (params.claim ?? true) ? getCoordinator(ctx.cwd) : undefined,
        signal,
      });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });

  pi.on("session_shutdown", async () => {
    await coordinator?.stop();
  });
}
