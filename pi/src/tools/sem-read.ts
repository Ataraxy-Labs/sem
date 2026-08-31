import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFile } from "node:fs/promises";
import { isAbsolute, relative, resolve } from "node:path";
import { Type } from "typebox";
import { extractEntities, resolveEntity, type Entity, type ResolveResult } from "./internal/entities.ts";
import { runCommand } from "./internal/proc.ts";

/**
 * Entity-addressed source read: return just one named entity's source instead
 * of a whole file, optionally plus its dependency-graph neighbors.
 *
 * Two ways to get content:
 *
 * - hops=0 (default): never shell out to `sem context` at all. sem context's
 *   own `--hops 0` means UNBOUNDED traversal (verified against sem 0.23.0), so
 *   forwarding our default would silently pull in related content the caller
 *   didn't ask for. Instead we slice the resolved entity's own lines straight
 *   out of the file ("direct-slice").
 * - hops>0: address `sem context --entity-id <id>` precisely, where `<id>` is
 *   computed from the already-resolved entity (`parent_id::name` for nested
 *   entities, `<relPath>::<type>::<name>` for top-level ones). If that call
 *   still fails or disagrees with what we resolved, fall back to the direct
 *   slice and say so ("direct-slice-fallback").
 *
 * All sem calls use a path relative to cwd: sem's internal path bookkeeping
 * only round-trips for cwd-relative paths (absolute inputs come back mangled
 * in parent_id/entity ids). Only our own filesystem reads use the absolute
 * path.
 */

export interface SemReadEntityRef {
  name: string;
  entity_type?: string;
  parent_name?: string;
  ordinal?: number;
}

export interface SemReadEntityRequest extends SemReadEntityRef {
  /** Optional — omit to auto-locate the entity repo-wide via `sem find`. Exactly one match reads it; several refuse with a candidate list; zero reports not-found. */
  file?: string;
}

export interface SemReadParams {
  /** Required unless `entities` is given. */
  entity?: SemReadEntityRef;
  /** Optional — omit to auto-locate `entity` repo-wide via `sem find`. */
  file?: string;
  /** Read several entities in one call instead of one call per entity. Each gets its own `budget` (default 1500), subject to a total ~6000-token cap across the whole batch. Takes priority over entity=/file= when non-empty. */
  entities?: SemReadEntityRequest[];
  budget?: number;
  hops?: number;
  /** "full" (default) returns the entity's whole source; "headers" returns just its signature line(s) plus a leading doc comment/docstring first line (<=3 lines), the middle zoom between sem_outline and a full read. Applies to entities= uniformly too. */
  mode?: "full" | "headers";
}

export interface SemReadDeps {
  cwd: string;
  semBin: string;
  signal?: AbortSignal;
}

export interface SemReadOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

const DEFAULT_BUDGET = 1500;

const SemReadEntityRefSchema = Type.Object({
  name: Type.String({ description: "Exact name of the function, class, method, etc. to read." }),
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
      description: "0-based occurrence index, in file order, among matches still ambiguous after entity_type/parent_name. Last resort — prefer entity_type/parent_name.",
    }),
  ),
});

const SemReadEntityRequestSchema = Type.Intersect([
  SemReadEntityRefSchema,
  Type.Object({ file: Type.Optional(Type.String({ description: "Omit to auto-locate this entity repo-wide via sem find." })) }),
]);

const SemReadParamsSchema = Type.Object({
  file: Type.Optional(
    Type.String({
      description: "Path to the file containing the entity, relative to the working directory or absolute. Omit to auto-locate repo-wide via sem find.",
    }),
  ),
  entity: Type.Optional(SemReadEntityRefSchema),
  entities: Type.Optional(
    Type.Array(SemReadEntityRequestSchema, {
      description:
        "Read several entities (each its own {name, file?, entity_type?, parent_name?, ordinal?}) in one call instead of one call per entity. Each gets its own budget; the whole batch is capped at ~6000 tokens total. Takes priority over entity=/file= when given.",
    }),
  ),
  budget: Type.Optional(
    Type.Integer({
      minimum: 1,
      description: "~token ceiling for the returned source. Defaults to 1500; content past it is cut and reported as truncated.",
    }),
  ),
  hops: Type.Optional(
    Type.Integer({
      minimum: 0,
      description:
        "Dependency-graph hops of related entities (dependents/dependencies) to pull in alongside the target. Defaults to 0 — just the entity itself.",
    }),
  ),
  mode: Type.Optional(
    Type.Union([Type.Literal("full"), Type.Literal("headers")], {
      description: "'full' (default) returns the whole source; 'headers' returns just the signature + a leading doc summary (<=3 lines).",
    }),
  ),
});

/** Strips pi's leading-@ file-reference convention, same as weave_edit/sem_outline. */
function stripAtPrefix(file: string): string {
  return file.startsWith("@") ? file.slice(1) : file;
}

/** The entity's own stable sem id, constructible from what extractEntities gave us — zero lookups. */
function ownEntityId(entity: Entity, relPath: string): string {
  // parent_id IS the parent's own entity id (given directly by sem); nested
  // children append only their name, not their own type again.
  return entity.parent_id ? `${entity.parent_id}::${entity.name}` : `${relPath}::${entity.type}::${entity.name}`;
}

// Mirrors weave-edit.ts's formatCandidate/formatResolutionFailure wording so
// all sem tools refuse ambiguous/not-found lookups identically. Deliberately
// local: weave_edit carries coordination state this tool doesn't have.

function formatCandidate(e: Entity): string {
  const parent = e.parentName ? ` in ${e.parentName}` : "";
  return `  - ${e.name} (${e.type}${parent}), lines ${e.start_line}-${e.end_line}`;
}

function formatResolutionFailure(req: SemReadEntityRequest, result: Extract<ResolveResult, { kind: "not-found" | "ambiguous" }>): SemReadOutcome {
  if (result.kind === "not-found") {
    const nearest = result.nearest.length > 0 ? ` Closest names in ${req.file}: ${result.nearest.join(", ")}.` : "";
    return {
      isError: true,
      text: `sem_read: no entity named "${req.name}" found in ${req.file}.${nearest}`,
      details: { file: req.file, entity: req, resolved: false, nearest: result.nearest },
    };
  }

  const list = result.candidates.map(formatCandidate).join("\n");
  return {
    isError: true,
    text: `sem_read: "${req.name}" is ambiguous in ${req.file} — ${result.candidates.length} matches. Add entity_type, parent_name, or ordinal to disambiguate:\n${list}`,
    details: {
      file: req.file,
      entity: req,
      resolved: false,
      candidates: result.candidates.map((e) => ({
        name: e.name,
        type: e.type,
        parent_name: e.parentName,
        start_line: e.start_line,
        end_line: e.end_line,
      })),
    },
  };
}

interface RawFindHit {
  id?: string;
  name: string;
  type: string;
  file: string;
  start_line: number;
  end_line: number;
}

/**
 * Runs `sem find <query> --json` repo-wide (no --file), used only to
 * discover which file an entity lives in when the caller omitted `file`.
 * Deliberately local rather than importing sem-find.ts's own copy — same
 * "mirrors weave-edit.ts's wording, deliberately local" convention already
 * used above for formatCandidate/formatResolutionFailure.
 */
async function runSemFindJson(query: string, deps: SemReadDeps): Promise<RawFindHit[]> {
  const result = await runCommand(deps.semBin, ["find", query, "--json"], deps.cwd, deps.signal);
  let hits: RawFindHit[];
  try {
    hits = JSON.parse(result.stdout) as RawFindHit[];
  } catch (err) {
    throw new Error(
      `"${deps.semBin} find ${JSON.stringify(query)} --json" failed (exit ${result.exitCode}): ${
        result.stderr.trim() || (err instanceof Error ? err.message : String(err))
      }`,
    );
  }
  return Array.isArray(hits) ? hits : [];
}

/**
 * sem's entity ids are `parent_id::name`, recursively — so for a nested
 * entity the segment immediately before the final name segment is always
 * its immediate parent's own name, regardless of nesting depth. A
 * top-level id (`file::type::name`, 3 segments) has no parent.
 */
function parseParentNameFromId(id: string | undefined): string | null {
  if (id === undefined) return null;
  const parts = id.split("::");
  return parts.length > 3 ? (parts[parts.length - 2] ?? null) : null;
}

function formatRepoCandidate(hit: RawFindHit): string {
  const parentName = parseParentNameFromId(hit.id);
  const parent = parentName ? ` in ${parentName}` : "";
  return `  - ${hit.name} (${hit.type}${parent}) — ${hit.file}:L${hit.start_line}-L${hit.end_line}`;
}

type FileResolution = { kind: "found"; file: string } | { kind: "not-found" } | { kind: "ambiguous"; hits: RawFindHit[] };

/** Locates which file a fileless request's entity lives in, repo-wide. Never picks among several matches — that's the caller's call, surfaced as an "ambiguous" refusal. */
async function resolveFileRepoWide(req: SemReadEntityRef, deps: SemReadDeps): Promise<FileResolution> {
  const query = req.entity_type !== undefined ? `${req.entity_type} ${req.name}` : req.name;
  const hits = await runSemFindJson(query, deps);
  if (hits.length === 0) return { kind: "not-found" };
  if (hits.length === 1) return { kind: "found", file: hits[0]!.file };
  return { kind: "ambiguous", hits };
}

/**
 * Slices the 1-indexed inclusive line range out of `content`, rejoining with
 * the file's dominant EOL. Local equivalent of internal/text.ts's
 * parseLines/renderLines pair (read-only, so no trailing-newline bookkeeping).
 */
function sliceEntitySource(content: string, startLine: number, endLine: number): string {
  const crlfCount = (content.match(/\r\n/g) ?? []).length;
  const bareLfCount = (content.match(/(?<!\r)\n/g) ?? []).length;
  const eol: "\n" | "\r\n" = crlfCount > bareLfCount ? "\r\n" : "\n";

  const normalized = content.replace(/\r\n/g, "\n");
  const body = normalized.endsWith("\n") ? normalized.slice(0, -1) : normalized;
  const lines = body.length === 0 ? [] : body.split("\n");

  return lines.slice(startLine - 1, endLine).join(eol);
}

/** Same EOL-detection/normalization as sliceEntitySource, but returns the whole file as a lines array — headers mode needs to look above `entity.start_line` for a doc comment, which a single-range slice can't reach. */
function normalizedFileLines(content: string): { lines: string[]; eol: "\n" | "\r\n" } {
  const crlfCount = (content.match(/\r\n/g) ?? []).length;
  const bareLfCount = (content.match(/(?<!\r)\n/g) ?? []).length;
  const eol: "\n" | "\r\n" = crlfCount > bareLfCount ? "\r\n" : "\n";
  const normalized = content.replace(/\r\n/g, "\n");
  const body = normalized.endsWith("\n") ? normalized.slice(0, -1) : normalized;
  return { lines: body.length === 0 ? [] : body.split("\n"), eol };
}

/** Hard cap on rendered lines in headers mode — the middle zoom between sem_outline and a full sem_read. */
const MAX_HEADER_LINES = 3;

/**
 * The doc-comment line immediately preceding `entityStartLine` (1-indexed),
 * or null when there isn't one — no blank-line gap allowed (a comment
 * separated from the entity by a blank line documents something else) and
 * only two shapes are recognized: a JSDoc-style `/** ... *\/` block (its
 * first non-empty content line) and a single trailing `//`/`///`/`#` line.
 * Deliberately narrow, same "honest heuristic, not a real parser" spirit as
 * internal/identity.ts's deriveVisibility.
 */
function leadingDocSummary(fileLines: string[], entityStartLine: number): string | null {
  const aboveIdx = entityStartLine - 2; // 0-indexed line immediately above start_line
  if (aboveIdx < 0) return null;
  const aboveLine = fileLines[aboveIdx] ?? "";
  if (aboveLine.trim().length === 0) return null;

  if (aboveLine.trim().endsWith("*/")) {
    let blockStart = aboveIdx;
    while (blockStart >= 0 && !/^\s*\/\*\*?/.test(fileLines[blockStart] ?? "")) blockStart--;
    if (blockStart < 0) return null;
    for (let i = blockStart; i <= aboveIdx; i++) {
      const stripped = (fileLines[i] ?? "")
        .replace(/^\s*\/\*\*?/, "")
        .replace(/\*\/\s*$/, "")
        .replace(/^\s*\*\s?/, "")
        .trim();
      if (stripped.length > 0) return stripped;
    }
    return null;
  }

  const lineComment = aboveLine.trim().match(/^(?:\/\/\/?|#)\s*(.*)$/);
  return lineComment && lineComment[1]!.length > 0 ? lineComment[1]!.trim() : null;
}

/** Signature lines for a brace/colon/equals-bodied entity: everything up to and including the first line whose trimmed text ends with the body's opening delimiter — handles both single- and multi-line signatures. No opener found (e.g. a markdown heading, a toml section) falls back to just the first line. */
function extractSignatureLines(entityLines: string[]): string[] {
  const openerIdx = entityLines.findIndex((l) => /[{:=]\s*$/.test(l.trimEnd()));
  return entityLines.slice(0, (openerIdx === -1 ? 0 : openerIdx) + 1);
}

/** Python's docstring is the body's first statement, not a comment above the def — "def line + docstring's first line" per the header-mode spec. */
function extractPythonHeader(entityLines: string[]): string[] {
  const defLine = entityLines[0] ?? "";
  const rest = entityLines.slice(1);
  const firstNonBlankIdx = rest.findIndex((l) => l.trim().length > 0);
  if (firstNonBlankIdx === -1) return [defLine];

  const firstNonBlank = rest[firstNonBlankIdx]!.trim();
  const tripleMatch = firstNonBlank.match(/^(?:"""|''')(.*)$/);
  if (!tripleMatch) return [defLine];

  if (tripleMatch[1]!.length > 0) {
    const summary = tripleMatch[1]!.replace(/(?:"""|''')\s*$/, "").trim();
    return summary.length > 0 ? [defLine, summary] : [defLine];
  }
  // Opening triple-quote alone on its own line — the summary is the next one.
  const summaryLine = rest[firstNonBlankIdx + 1]?.trim();
  return summaryLine ? [defLine, summaryLine] : [defLine];
}

/**
 * Just the entity's signature line(s) plus a leading doc summary, capped at
 * MAX_HEADER_LINES — the "headers" mode content. When both a doc summary
 * and a signature exist, the doc line goes first (matching normal reading
 * order) and the cap is applied to the combined list, so a long signature
 * doesn't push the doc line out but a long doc-plus-signature combination
 * still respects the 3-line ceiling.
 */
function extractHeader(fileLines: string[], entity: Entity, filePath: string): { lines: string[]; truncated: boolean } {
  const entityLines = fileLines.slice(entity.start_line - 1, entity.end_line);

  if (filePath.endsWith(".py")) {
    const lines = extractPythonHeader(entityLines);
    const capped = lines.slice(0, MAX_HEADER_LINES);
    return { lines: capped, truncated: lines.length > capped.length };
  }

  const signature = extractSignatureLines(entityLines);
  const doc = leadingDocSummary(fileLines, entity.start_line);
  const combined = doc !== null ? [doc, ...signature] : signature;
  const capped = combined.slice(0, MAX_HEADER_LINES);
  return { lines: capped, truncated: combined.length > capped.length };
}

/**
 * `.slice()` cuts by UTF-16 code unit, not code point — a cut that lands
 * inside an astral-plane character's surrogate pair (most emoji, some CJK
 * Extension B+, math alphanumeric symbols) leaves a lone unpaired high
 * surrogate at the end, which is malformed UTF-16 and can corrupt whatever
 * re-encodes this string downstream (e.g. JSON for the model response).
 */
function trimTrailingUnpairedSurrogate(s: string): string {
  const last = s.charCodeAt(s.length - 1);
  return last >= 0xd800 && last <= 0xdbff ? s.slice(0, -1) : s;
}

/**
 * Direct-slice budget enforcement on the actual sliced text (~4 bytes/token):
 * cut to roughly the budget's worth of characters when over. sem enforces its
 * own budget on the sem-context path; this covers every path we cut ourselves.
 */
function enforceBudget(content: string, budget: number): { content: string; tokens: number; truncated: boolean } {
  const tokens = Math.round(content.length / 4);
  if (tokens <= budget) return { content, tokens, truncated: false };
  const sliced = trimTrailingUnpairedSurrogate(content.slice(0, Math.max(1, budget * 4)));
  return { content: sliced, tokens: budget, truncated: true };
}

interface SemContextEntry {
  content: string;
  entityId: string;
  file: string;
  name: string;
  role: string;
  type: string;
  tokens: number;
}

interface SemContextJson {
  entries: SemContextEntry[];
  total_tokens: number;
  truncated: boolean;
}

async function runSemContext(
  semBin: string,
  cwd: string,
  relPath: string,
  entityId: string,
  budget: number,
  hops: number,
  signal?: AbortSignal,
): Promise<SemContextJson> {
  const result = await runCommand(
    semBin,
    ["context", "--entity-id", entityId, "--file", relPath, "--budget", String(budget), "--hops", String(hops), "--format", "json"],
    cwd,
    signal,
  );
  if (result.exitCode !== 0) {
    throw new Error(`"${semBin} context --entity-id ${entityId}" failed (exit ${result.exitCode}): ${result.stderr.trim() || "no stderr output"}`);
  }
  return JSON.parse(result.stdout) as SemContextJson;
}

function describeEntity(e: Entity): string {
  return `${e.name} (${e.type}${e.parentName ? ` in ${e.parentName}` : ""})`;
}

function buildSuccessText(req: SemReadEntityRequest, hops: number, entity: Entity, content: string, relatedNames: string[], rangeSource: string, note?: string): string {
  const header = `sem_read: ${describeEntity(entity)} in ${req.file}, lines ${entity.start_line}-${entity.end_line}${note ? ` — ${note}` : ""}`;
  const related = relatedNames.length > 0 ? `\nRelated entities (hops=${hops}): ${relatedNames.join(", ")}.` : "";
  const provenance = `\n[source: ${rangeSource}]`;
  return `${header}${related}\n\n${content}${provenance}`;
}

/** Every rendered entity's outcome, plus how many ~tokens it actually spent (0 for a resolution/execution failure), for the batch cap below to track. */
interface EntityReadResult {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
  tokens: number;
}

/**
 * One entity's full read. When `req.file` is omitted, first locates it
 * repo-wide via sem find — turning the usual outline-then-read ritual for
 * "show me function X" into a single call. Exactly one repo-wide match
 * reads it directly; several always refuse with a candidate list (never
 * picks the first); zero reports not-found. sem find offers no
 * spelling-suggestion mechanism (confirmed empirically — its JSON is a
 * plain hit array, nothing like entities.ts's own nearest-name fallback),
 * so the not-found message doesn't claim to have checked for one.
 */
async function readOneEntity(req: SemReadEntityRequest, budget: number, hops: number, mode: "full" | "headers", deps: SemReadDeps): Promise<EntityReadResult> {
  if (req.file !== undefined) {
    return readOneEntityWithFile({ ...req, file: req.file }, budget, hops, mode, deps);
  }

  try {
    const resolution = await resolveFileRepoWide(req, deps);

    if (resolution.kind === "not-found") {
      return {
        isError: true,
        text: `sem_read: no entity named "${req.name}" found anywhere in the repo (sem find: exact, case-sensitive matching, no fuzzy suggestions available).`,
        details: { entity: req, resolved: false },
        tokens: 0,
      };
    }

    if (resolution.kind === "ambiguous") {
      const list = resolution.hits.map(formatRepoCandidate).join("\n");
      return {
        isError: true,
        text: `sem_read: "${req.name}" is ambiguous across the repo — ${resolution.hits.length} matches. Pass file (and parent_name/ordinal if still ambiguous within it) to disambiguate:\n${list}`,
        details: {
          entity: req,
          resolved: false,
          candidates: resolution.hits.map((h) => ({
            name: h.name,
            type: h.type,
            file: h.file,
            parent_name: parseParentNameFromId(h.id),
            start_line: h.start_line,
            end_line: h.end_line,
          })),
        },
        tokens: 0,
      };
    }

    return readOneEntityWithFile({ ...req, file: resolution.file }, budget, hops, mode, deps);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_read: failed to locate ${req.name} repo-wide: ${message}`,
      details: { entity: req, error: message },
      tokens: 0,
    };
  }
}

/**
 * One entity's full read once its file is known, independent of the
 * entity=/file= vs entities= dispatch below.
 *
 * Dogfood round 2 finding: every success branch's `details` now
 * carries the BARE entity source as `content`, separate from `.text`
 * (`buildSuccessText`'s header + related-entities line + body + `[source:
 * ...]` footer, meant for a human/LLM reading a tool result, never for
 * feeding back into code). codemode's api.ts#readOne used to set its
 * ReadResult.content to `outcome.text` for lack of anywhere else to get the
 * entity's source from performSemRead's public shape — confirmed via a real
 * dogfood run that a script doing `read.content.trimEnd() + "\n\n" + docs`
 * followed by `sem.edit(..., content)` spliced the WHOLE decorated report
 * (the "sem_read: ... lines N-M" header and "[source: ...]" footer
 * included) into the file in place of the entity, breaking it enough that
 * post-edit re-extraction found no entity there at all — read as a
 * confusing "changes its name" identity-check refusal, not what the actual
 * defect (garbled written content) was. `details.content` gives api.ts (and
 * any other consumer) the clean, reusable source directly, with `.text`
 * unchanged for the native sem_read tool's own human-facing output.
 */
async function readOneEntityWithFile(
  req: SemReadEntityRequest & { file: string },
  budget: number,
  hops: number,
  mode: "full" | "headers",
  deps: SemReadDeps,
): Promise<EntityReadResult> {
  const { cwd, semBin, signal } = deps;

  const stripped = stripAtPrefix(req.file);
  const absPath = resolve(cwd, stripped);
  // sem's path bookkeeping round-trips only for cwd-relative paths, so every
  // sem invocation uses relPath even when the caller passed an absolute path.
  // absPath is reserved for our own readFile calls below.
  const relPath = isAbsolute(stripped) ? relative(cwd, stripped) : stripped;

  try {
    const entities = await extractEntities(semBin, relPath, cwd, signal);
    const resolved = resolveEntity(entities, req);

    if (resolved.kind !== "found") {
      return { ...formatResolutionFailure(req, resolved), tokens: 0 };
    }

    const entity = resolved.entity;

    if (mode === "headers") {
      // Headers mode is a lighter, faster zoom level than a full read — it
      // never traverses the dependency graph, so hops has no effect here
      // regardless of what the caller passed.
      const { lines: fileLines, eol } = normalizedFileLines(await readFile(absPath, "utf8"));
      const header = extractHeader(fileLines, entity, req.file);
      const content = header.lines.join(eol);
      return {
        isError: false,
        text: buildSuccessText(req, hops, entity, content, [], "headers", header.truncated ? "truncated to fit the 3-line header cap" : undefined),
        details: {
          file: req.file,
          entity: {
            name: entity.name,
            type: entity.type,
            parent_name: entity.parentName,
            start_line: entity.start_line,
            end_line: entity.end_line,
          },
          range_source: "headers",
          related: [],
          truncated: header.truncated,
          mode: "headers",
          content,
        },
        tokens: Math.round(content.length / 4),
      };
    }

    if (hops > 0) {
      let contextFailure: string | undefined;
      try {
        const json = await runSemContext(semBin, cwd, relPath, ownEntityId(entity, relPath), budget, hops, signal);
        const target = json.entries.find((entry) => entry.role === "target");
        const matchesResolution =
          target !== undefined &&
          target.name === entity.name &&
          target.type === entity.type &&
          relative(cwd, resolve(cwd, stripAtPrefix(target.file))) === relPath;

        if (!matchesResolution || target === undefined) {
          contextFailure = "sem context's target entry disagreed with our own resolution";
        } else {
          const related = json.entries.filter((entry) => entry.role !== "target");
          return {
            isError: false,
            text: buildSuccessText(
              req,
              hops,
              entity,
              target.content,
              related.map((r) => `${r.name} (${r.type}, role: ${r.role})`),
              "sem-context",
            ),
            details: {
              file: req.file,
              entity: {
                name: entity.name,
                type: entity.type,
                parent_name: entity.parentName,
                start_line: entity.start_line,
                end_line: entity.end_line,
              },
              entity_id: ownEntityId(entity, relPath),
              range_source: "sem-context",
              related: related.map((r) => ({ name: r.name, type: r.type, file: r.file, role: r.role })),
              total_tokens: json.total_tokens,
              truncated: json.truncated,
              budget,
              content: target.content,
            },
            tokens: json.total_tokens,
          };
        }
      } catch (err) {
        contextFailure = err instanceof Error ? err.message : String(err);
      }

      // sem context couldn't take an address precise enough (non-zero exit,
      // unparseable JSON, or a target disagreeing with our resolution).
      const slicedRaw = sliceEntitySource(await readFile(absPath, "utf8"), entity.start_line, entity.end_line);
      const enforced = enforceBudget(slicedRaw, budget);
      return {
        isError: false,
        text: buildSuccessText(
          req,
          hops,
          entity,
          enforced.content,
          [],
          "direct-slice-fallback",
          `hops=${hops} was requested but sem context couldn't be used, so no related entities are included`,
        ),
        details: {
          file: req.file,
          entity: {
            name: entity.name,
            type: entity.type,
            parent_name: entity.parentName,
            start_line: entity.start_line,
            end_line: entity.end_line,
          },
          entity_id: ownEntityId(entity, relPath),
          range_source: "direct-slice-fallback",
          related: [],
          truncated: enforced.truncated,
          budget,
          requested_hops: hops,
          context_error: contextFailure ?? "unknown reason",
          content: enforced.content,
        },
        tokens: enforced.tokens,
      };
    }

    // hops=0: never call sem context (its --hops 0 means unbounded). Slice
    // the entity's own lines ourselves so no related content leaks in.
    const slicedRaw = sliceEntitySource(await readFile(absPath, "utf8"), entity.start_line, entity.end_line);
    const enforced = enforceBudget(slicedRaw, budget);
    return {
      isError: false,
      text: buildSuccessText(req, hops, entity, enforced.content, [], "direct-slice"),
      details: {
        file: req.file,
        entity: {
          name: entity.name,
          type: entity.type,
          parent_name: entity.parentName,
          start_line: entity.start_line,
          end_line: entity.end_line,
        },
        range_source: "direct-slice",
        related: [],
        truncated: enforced.truncated,
        budget,
        content: enforced.content,
      },
      tokens: enforced.tokens,
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return {
      isError: true,
      text: `sem_read: failed to read ${req.name} from ${req.file}: ${message}`,
      details: { file: req.file, entity: req, error: message },
      tokens: 0,
    };
  }
}

/** Hard cap on ~tokens rendered across an entire entities= batch (eval-driven, same motivation as the per-entity default). */
const MAX_TOTAL_BUDGET = 6000;

export async function performSemRead(params: SemReadParams, deps: SemReadDeps): Promise<SemReadOutcome> {
  const hops = params.hops ?? 0;
  const perEntityBudget = params.budget ?? DEFAULT_BUDGET;
  const mode = params.mode ?? "full";

  const entities = params.entities;
  if (entities === undefined || entities.length === 0) {
    if (params.entity === undefined) {
      return { isError: true, text: "sem_read: pass either { entity, file? } or entities=[...].", details: { error: "missing entity" } };
    }
    const one = await readOneEntity({ ...params.entity, file: params.file }, perEntityBudget, hops, mode, deps);
    return { isError: one.isError, text: one.text, details: one.details };
  }

  // Each entity gets perEntityBudget (default 1500), but the whole batch is
  // capped at MAX_TOTAL_BUDGET total — an entity reached once that's already
  // spent is honestly reported as skipped rather than silently rendered in
  // full (which would blow past the very token budget entities= exists to
  // control) or silently dropped (which would hide that anything was cut).
  const results: EntityReadResult[] = [];
  let totalTokens = 0;
  for (const req of entities) {
    if (totalTokens >= MAX_TOTAL_BUDGET) {
      results.push({
        isError: false,
        tokens: 0,
        text: `sem_read: ${req.name}${req.file ? ` in ${req.file}` : ""} — not read; this call's total budget (${MAX_TOTAL_BUDGET} tokens) was already spent by earlier entities.`,
        details: { file: req.file ?? null, entity: req, skipped: true, truncated: true },
      });
      continue;
    }
    const remainingBudget = Math.max(1, Math.min(perEntityBudget, MAX_TOTAL_BUDGET - totalTokens));
    const one = await readOneEntity(req, remainingBudget, hops, mode, deps);
    totalTokens += one.tokens;
    results.push(one);
  }

  const allFailed = results.every((r) => r.isError);
  return {
    isError: allFailed,
    text: results.map((r) => r.text).join("\n\n---\n\n"),
    details: {
      count: entities.length,
      total_tokens: totalTokens,
      budget_per_entity: perEntityBudget,
      total_budget_cap: MAX_TOTAL_BUDGET,
      results: results.map((r) => r.details),
    },
  };
}

export interface RegisterSemReadOptions {
  /** `sem` binary to shell out to for extraction/context. Defaults to "sem" (resolved via PATH). */
  semBin?: string;
}

/** Registers the `sem_read` tool. Call once per pi extension load. */
export function registerSemRead(pi: ExtensionAPI, opts: RegisterSemReadOptions = {}): void {
  const semBin = opts.semBin ?? "sem";

  pi.registerTool({
    name: "sem_read",
    label: "Sem Read",
    description:
      "Read one entity by name (file optional; auto-locates it). mode=headers for just the signature. entities=[...] batches several reads.",
    promptSnippet: "Read one named entity's source (optionally with its dependents/dependencies) instead of a whole file",
    promptGuidelines: [
      "Skip sem_outline/sem_find for the common \"show me function X\" ask — sem_read works with just entity.name (no file=): one repo-wide match reads it directly, several refuse with a candidate list to disambiguate, none reports not-found.",
      "Pass hops>0 to also pull in the entity's direct dependents/dependencies; leave it at its default (0) otherwise — related content costs extra budget you didn't ask to spend.",
      "Use mode=\"headers\" for the middle zoom between sem_outline and a full read: just each entity's signature line(s) plus a leading doc summary/docstring, capped at 3 lines — good for skimming several candidates before committing to a full read.",
      "If the entity name exists in more than one place, add entity_type/parent_name/ordinal; sem_read refuses with the candidate list rather than guessing.",
    ],
    parameters: SemReadParamsSchema,
    async execute(_toolCallId, params, signal, _onUpdate, ctx) {
      const outcome = await performSemRead(params, { cwd: ctx.cwd, semBin, signal });
      if (outcome.isError) throw new Error(outcome.text);
      return { content: [{ type: "text", text: outcome.text }], details: outcome.details };
    },
  });
}
