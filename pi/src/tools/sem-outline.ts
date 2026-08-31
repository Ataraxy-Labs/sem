import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { Type } from "typebox";
import { extractEntities, type Entity } from "./internal/entities.ts";

export interface SemOutlineParams {
  file: string;
  text?: string;
  depth?: number;
}

export interface SemOutlineDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemOutlineOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

const SemOutlineParamsSchema = Type.Object({
  file: Type.String({ description: "Path to the file to outline, relative to the working directory or absolute." }),
  text: Type.Optional(
    Type.String({
      description:
        "Case-insensitive substring to filter entity names by. Matching entities are shown with enough ancestor context (their parent chain) to locate them; header totals still describe the whole file.",
    }),
  ),
  depth: Type.Optional(
    Type.Integer({
      minimum: 1,
      description:
        "1-indexed maximum nesting depth to render: depth=1 shows only top-level entities, depth=2 adds their direct children, etc. Omit for unlimited.",
    }),
  ),
});

/** Hard cap on rendered entity lines (the header and any trailing hint don't count). */
const MAX_RENDERED_LINES = 120;

function resolveTargetPath(cwd: string, file: string): string {
  const stripped = file.startsWith("@") ? file.slice(1) : file;
  return resolve(cwd, stripped);
}

/**
 * Per-entity size in ~tokens. Byte-derived when sem returned a usable byte
 * range (~4 bytes/token, via Entity.byteRangeReliable — see
 * internal/entities.ts's deriveEntity); otherwise falls back to a
 * line-count heuristic. Whether non-code entities (markdown headings,
 * toml/yaml sections, ...) carry a byte range at all is sem-version-
 * dependent (confirmed empirically: sem 0.23.0 omits it for them, 0.23.1+
 * includes one) — this function doesn't care which shape it got, only
 * whether byteRangeReliable says the range can be trusted.
 */
export function tokenEstimate(e: Entity): number {
  if (e.byteRangeReliable) {
    // byteRangeReliable true means both bytes are present numbers.
    return Math.max(1, Math.round((e.end_byte! - e.start_byte!) / 4));
  }
  return (e.end_line - e.start_line + 1) * 10;
}

/**
 * Resolves which Entity is e's parent for tree rendering. Name-based first
 * (sem's parent_id already carries the parent's name as its final
 * `::`-segment), then geometry only as a tie-breaker among same-named
 * candidates — pure geometric containment alone fails for markdown, where a
 * heading's own line range does NOT enclose its subsections even though sem
 * reports the subsections' parent as that heading.
 */
function findParent(entities: Entity[], e: Entity): Entity | null {
  if (e.parentName === null) return null;

  const candidates = entities.filter((c) => c.name === e.parentName);
  if (candidates.length === 0) return null; // dangling parent name -> render as top-level
  if (candidates.length === 1) return candidates[0]!;

  // Code case: several same-named candidates (two classes both named Foo) —
  // pick the tightest one whose line range geometrically contains e.
  const containing = candidates.filter(
    (c) => c !== e && c.start_line <= e.start_line && c.end_line >= e.end_line,
  );
  if (containing.length > 0) {
    return containing.reduce((best, c) => {
      const bestSpan = best.end_line - best.start_line;
      const span = c.end_line - c.start_line;
      if (span < bestSpan) return c;
      if (span === bestSpan && c.start_line < best.start_line) return c;
      return best;
    });
  }

  // Markdown/disjoint case with duplicate names: nearest candidate whose
  // start_line is before e's (later-in-file wins ties), else first candidate.
  const before = candidates.filter((c) => c !== e && c.start_line < e.start_line);
  if (before.length > 0) {
    return before.reduce((best, c) => (c.start_line >= best.start_line ? c : best));
  }
  return candidates[0]!;
}

interface OutlineTree {
  roots: Entity[];
  childrenOf: Map<Entity, Entity[]>;
}

/** Groups children under each resolved parent; siblings keep natural (start_line) order. */
function buildTree(entities: Entity[]): OutlineTree {
  const childrenOf = new Map<Entity, Entity[]>();
  const roots: Entity[] = [];

  for (const e of entities) {
    const parent = findParent(entities, e);
    if (parent === null) {
      roots.push(e);
    } else {
      const siblings = childrenOf.get(parent);
      if (siblings) siblings.push(e);
      else childrenOf.set(parent, [e]);
    }
  }

  for (const siblings of childrenOf.values()) {
    siblings.sort((a, b) => a.start_line - b.start_line);
  }
  roots.sort((a, b) => a.start_line - b.start_line);

  return { roots, childrenOf };
}

export async function performSemOutline(params: SemOutlineParams, deps: SemOutlineDeps): Promise<SemOutlineOutcome> {
  const { cwd, semBin, signal } = deps;
  const absPath = resolveTargetPath(cwd, params.file);

  let entities: Entity[];
  let wholeFileTokens: number;
  try {
    const [extracted, fileText] = await Promise.all([
      extractEntities(semBin, absPath, cwd, signal),
      readFile(absPath, "utf8"),
    ]);
    entities = extracted;
    // Whole-file estimate straight from raw byte length (~4 bytes/token) —
    // deliberately NOT a sum of per-entity estimates, which double-counts or
    // under-counts depending on language (markdown/toml entity ranges are
    // often disjoint from their nominal parent heading's range).
    wholeFileTokens = Math.round(Buffer.byteLength(fileText, "utf8") / 4);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_outline: failed to outline ${params.file}: ${message}`,
      details: { file: params.file, error: message },
    };
  }

  const tree = buildTree(entities);

  // text= filter: keep an entity if its own name matches (case-insensitive
  // substring) OR any descendant matches, so ancestors stay visible as
  // context. Computed bottom-up.
  let shownCount = 0;
  let keptPredicate: (e: Entity) => boolean;
  if (params.text === undefined) {
    keptPredicate = () => true;
    shownCount = entities.length;
  } else {
    const needle = params.text.toLowerCase();
    const keptMemo = new Map<Entity, boolean>();
    keptPredicate = (e: Entity): boolean => {
      const cached = keptMemo.get(e);
      if (cached !== undefined) return cached;
      const kept =
        e.name.toLowerCase().includes(needle) ||
        (tree.childrenOf.get(e) ?? []).some((child) => keptPredicate(child));
      keptMemo.set(e, kept);
      if (kept) shownCount++;
      return kept;
    };
    for (const root of tree.roots) keptPredicate(root);
  }

  // An entity whose own name matches text= — depth= caps how much tree gets
  // rendered, but it must never suppress the very thing text= exists to
  // find (an ancestor chain already gets this exception via keptPredicate;
  // the match itself needs the same treatment, or a search for something
  // that happens to live past depth= silently finds nothing).
  const needle = params.text?.toLowerCase();
  const isOwnMatch = (e: Entity): boolean => needle !== undefined && e.name.toLowerCase().includes(needle);

  // Gather every kept entity with its pre-order position and 1-indexed tree
  // depth (roots at depth 1). params.depth, if given, is a hard ceiling the
  // user asked for — entities beyond it are collected only when they are
  // themselves a text= match; the collapse logic below preserves that same
  // exception so the auto-shrink cap can't hide a match either.
  interface Entry {
    entity: Entity;
    depth: number;
    isMatch: boolean;
  }
  const allEntries: Entry[] = [];
  let maxDepthSeen = 0;

  const collect = (e: Entity, depth: number): void => {
    const withinCeiling = params.depth === undefined || depth <= params.depth;
    const ownMatch = isOwnMatch(e);
    if (withinCeiling || ownMatch) {
      allEntries.push({ entity: e, depth, isMatch: ownMatch });
      if (depth > maxDepthSeen) maxDepthSeen = depth;
    }
    // Past the ceiling, only keep descending when text= is active and this
    // subtree still has an unrendered match somewhere below (keptPredicate
    // already knows that) — otherwise stop, same as the plain depth= case.
    if (!withinCeiling && params.text === undefined) return;
    for (const child of tree.childrenOf.get(e) ?? []) {
      if (keptPredicate(child)) collect(child, depth + 1);
    }
  };

  for (const root of tree.roots) {
    if (keptPredicate(root)) collect(root, 1);
  }

  // Whole nesting levels collapse before anything gets an arbitrary
  // mid-tree cutoff: try rendering at the full depth first, then shed the
  // deepest level and retry, until the kept set fits the cap. Only once
  // there's no nesting left to shed (D=1) does a file with more than
  // MAX_RENDERED_LINES top-level entities fall back to a line cutoff. A
  // text= match is exempt from being shed by this loop for the same reason
  // it's exempt from the depth= ceiling above.
  let renderDepth = maxDepthSeen;
  while (renderDepth > 1 && allEntries.filter((entry) => entry.depth <= renderDepth || entry.isMatch).length > MAX_RENDERED_LINES) {
    renderDepth--;
  }

  const withinDepth = allEntries.filter((entry) => entry.depth <= renderDepth || entry.isMatch);
  const shown = withinDepth.length <= MAX_RENDERED_LINES ? withinDepth : withinDepth.slice(0, MAX_RENDERED_LINES);
  const omitted = allEntries.length - shown.length;

  const lines = shown.map(({ entity: e, depth }) => {
    const indent = "  ".repeat(depth - 1);
    return `${indent}${e.type} ${e.name} [L${e.start_line}-L${e.end_line}] ~${tokenEstimate(e)}`;
  });

  const header =
    `${params.file}: ${entities.length} entities, ~${wholeFileTokens} tokens` +
    (params.text !== undefined ? ` (matching "${params.text}": ${shownCount} shown)` : "");

  const body = omitted > 0 ? [...lines, `…${omitted} more (use text= or depth=)`] : lines;

  return {
    isError: false,
    text: [header, ...body].join("\n"),
    details: {
      file: params.file,
      entity_count: entities.length,
      total_tokens: wholeFileTokens,
      entities: entities.map((e) => ({
        name: e.name,
        type: e.type,
        parent_name: e.parentName,
        start_line: e.start_line,
        end_line: e.end_line,
        tokens: tokenEstimate(e),
        byte_range_reliable: e.byteRangeReliable,
      })),
      truncated: omitted > 0,
      text_filter: params.text ?? null,
      depth_limit: params.depth ?? null,
    },
  };
}

export interface RegisterSemOutlineOptions {
  /** `sem` binary to shell out to for entity extraction. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_outline` tool. Call once per pi extension load. */
export function registerSemOutline(pi: ExtensionAPI, opts: RegisterSemOutlineOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_outline",
    label: "Sem Outline",
    description:
      "Sized, nested outline of a file's entities (functions/classes/headings/sections), with line ranges and ~token sizes. Filter with text=, cap with depth=.",
    promptSnippet: "Show one file's nested entity outline with sizes, before reading any of it",
    promptGuidelines: [
      'Use sem_outline first to see a file\'s structure with sizes, then read only the entity you need with sem_context/sem_read.',
      "Pass text= to narrow the outline to entities whose name contains a substring (ancestor context is kept so matches stay locatable), and depth= to skip nesting you don't care about.",
      "Prefer outlining over reading whole files: the header reports the file's total ~token cost up front, and each entity's estimate tells you exactly what reading it would spend.",
    ],
    parameters: SemOutlineParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemOutline(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
