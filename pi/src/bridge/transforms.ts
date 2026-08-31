/**
 * Pure, best-effort transforms over an MCP tool's raw text result, applied
 * before the result reaches the LLM. A config entry names one by id (see
 * `ToolOverride.transform` in src/config/types.ts); `TRANSFORMS[id]` looks
 * it up. An unknown id, or a transform that can't make sense of the text
 * it's given, must never throw or block the tool call — it degrades to
 * passing the original text through unchanged. `register.ts` keeps the
 * untransformed text available in the tool result's `details` regardless.
 */

export interface TransformContext {
  /** The MCP tool's own name (not any renamed/curated name pi shows the LLM). */
  toolName: string;
  /** The arguments the tool was called with (post leading-"@" normalization). */
  args: unknown;
}

export type ResultTransform = (text: string, ctx: TransformContext) => string;

/**
 * Entity shape "outline" expects from a tool's JSON result: a flat array of
 * entities, each naming its parent (if any) by the parent's `name` -- the
 * shape carries no separate `id` field for entities themselves, so `name`
 * is the only thing `parent_id` can coherently reference. `start_byte` /
 * `end_byte` are optional; when a pair is present, ~tokens is estimated as
 * round((end_byte - start_byte) / 4) and shown, otherwise omitted rather
 * than guessed.
 *
 * No MCP tool in this bridge emits exactly this shape today (verified
 * against live sem 0.23.0 and the rebuilt weave-mcp -- see the commit this
 * ships with for what each currently returns). This is deliberately built
 * to the documented target shape so it activates correctly the moment a
 * tool's result matches it, and degrades safely (passes raw text through)
 * on anything that doesn't parse as this shape today.
 */
interface OutlineEntity {
  type: string;
  name: string;
  parent_id?: string | null;
  start_line: number;
  end_line: number;
  start_byte?: number;
  end_byte?: number;
}

const OUTLINE_MAX_LINES = 200;

function isOutlineEntity(value: unknown): value is OutlineEntity {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return (
    typeof record.type === "string" &&
    typeof record.name === "string" &&
    typeof record.start_line === "number" &&
    typeof record.end_line === "number" &&
    (record.parent_id === undefined || record.parent_id === null || typeof record.parent_id === "string") &&
    (record.start_byte === undefined || typeof record.start_byte === "number") &&
    (record.end_byte === undefined || typeof record.end_byte === "number")
  );
}

function parseOutlineEntities(text: string): OutlineEntity[] | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return undefined;
  }
  if (!Array.isArray(parsed) || parsed.length === 0) return undefined;
  if (!parsed.every(isOutlineEntity)) return undefined;
  return parsed;
}

function estimateTokens(entity: OutlineEntity): number | undefined {
  if (entity.start_byte === undefined || entity.end_byte === undefined) return undefined;
  return Math.round((entity.end_byte - entity.start_byte) / 4);
}

const PATH_LIKE_ARG_NAMES = ["file_path", "path", "file"];

function fileFromArgs(args: unknown): string | undefined {
  if (typeof args !== "object" || args === null) return undefined;
  const record = args as Record<string, unknown>;
  for (const key of PATH_LIKE_ARG_NAMES) {
    const value = record[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return undefined;
}

function formatEntityLine(entity: OutlineEntity, depth: number): string {
  const indent = "  ".repeat(depth);
  const tokens = estimateTokens(entity);
  const tokenSuffix = tokens === undefined ? "" : ` ~${tokens}`;
  return `${indent}${entity.type} ${entity.name} [L${entity.start_line}-L${entity.end_line}]${tokenSuffix}`;
}

/**
 * Renders a flat entity array as an indented outline, nested by matching
 * each entity's `parent_id` to another entity's `name`. An entity whose
 * `parent_id` doesn't resolve to a known name (including entities with no
 * `parent_id` at all) renders at the top level -- never dropped, never a
 * crash on a dangling reference.
 */
function renderOutline(entities: OutlineEntity[], file: string | undefined): string {
  const byParent = new Map<string | undefined, OutlineEntity[]>();
  const knownNames = new Set(entities.map((e) => e.name));

  for (const entity of entities) {
    const parentKey = entity.parent_id && knownNames.has(entity.parent_id) ? entity.parent_id : undefined;
    const siblings = byParent.get(parentKey) ?? [];
    siblings.push(entity);
    byParent.set(parentKey, siblings);
  }

  const lines: string[] = [];
  let truncatedAt: number | undefined;

  function visit(parentKey: string | undefined, depth: number): void {
    for (const entity of byParent.get(parentKey) ?? []) {
      if (lines.length >= OUTLINE_MAX_LINES) {
        truncatedAt ??= entities.length - lines.length;
        return;
      }
      lines.push(formatEntityLine(entity, depth));
      visit(entity.name, depth + 1);
    }
  }
  visit(undefined, 0);

  const totalTokens = entities.reduce((sum, e) => sum + (estimateTokens(e) ?? 0), 0);
  const header = `${file ?? "(unknown file)"}: ${entities.length} entities, ~${totalTokens} tokens`;

  const body = truncatedAt !== undefined ? [...lines, `…${truncatedAt} more; filter with text=`] : lines;
  return [header, ...body].join("\n");
}

const outlineTransform: ResultTransform = (text, ctx) => {
  const entities = parseOutlineEntities(text);
  if (!entities) return text; // Not the shape "outline" expects -- pass through unchanged.
  return renderOutline(entities, fileFromArgs(ctx.args));
};

export const TRANSFORMS: Record<string, ResultTransform> = {
  outline: outlineTransform,
};

/** Applies a named transform if registered; unknown ids pass text through unchanged. */
export function applyTransform(id: string | undefined, text: string, ctx: TransformContext): string {
  if (!id) return text;
  const transform = TRANSFORMS[id];
  if (!transform) return text;
  return transform(text, ctx);
}
