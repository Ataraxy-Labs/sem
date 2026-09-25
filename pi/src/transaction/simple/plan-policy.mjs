export const MAX_PLAN_CALLS = 8;
export function boundedInteger(value, fallback, minimum, maximum) {
  if (value === undefined) return fallback;
  if (!Number.isInteger(value) || value < minimum) throw new Error(`expected integer >= ${minimum}`);
  return Math.min(value, maximum);
}
export function inScope(file, scope) {
  if (!scope || scope === ".") return true;
  const clean = scope.replace(/^\.\//, "").replace(/\/$/, "");
  return file === clean || file.startsWith(clean + "/");
}
export function candidatePage(hits, offset, size) {
  return { hits: hits.slice(offset, offset + size), total: hits.length,
    next_offset: offset + size < hits.length ? offset + size : null };
}
export async function focusedCheck(api, command) {
  try {
    return await api.check(command ? { cmd: command } : {});
  } catch (error) {
    return { pass: null, stage: "unavailable", requested_cmd_rejected: command ?? null,
      error: String(error instanceof Error ? error.message : error).slice(-2000),
      next_action: "Retry a runner-allowlisted focused command without shell/environment prefixes. No broader test suite was run." };
  }
}
export function compactEditReceipt(outcome) {
  const details = outcome.details ?? outcome;
  // Keep failures verbatim: recovery must not lose its diagnostics.
  if (outcome.isError || details.failed > 0 || details.rolledBack || details.results?.some(r => r.isError || r.outcome?.isError)) return details;
  return {
    succeeded: details.succeeded ?? details.applied ?? null,
    failed: details.failed ?? 0,
    rolledBack: details.rolledBack ?? false,
    dependents: { checked: false, reason: "Informational per-edit caller reporting omitted; correctness validation reported separately." },
    results: (details.results ?? []).map(result => ({
      file: result.file, entity: result.entity,
      isError: result.isError ?? result.outcome?.isError ?? false,
      verification: result.details?.verification ?? result.outcome?.details?.verification ?? null,
      merge: result.details?.merge ?? result.outcome?.details?.merge ?? null,
      coordination: result.details?.coordination ?? result.outcome?.details?.coordination ?? null,
    })),
  };
}
export function packDefinitions(definitions, byteBudget = 48000) {
  let used = 0, deferred = 0;
  const packed = definitions.map(definition => {
    const size = Buffer.byteLength(JSON.stringify(definition));
    if (used + size <= byteBudget) { used += size; return definition; }
    deferred++;
    return {file: definition.file, entity: definition.entity, truncated: true,
      deferred: true, reason: "Definition byte budget reached; query this entity and path directly."};
  });
  return {definitions: packed, full_definition_bytes: used, byte_budget: byteBudget, deferred};
}
export function compactDefinition(definition) {
  const { related, budget, ...result } = definition;
  // Omit only empty navigation metadata and redundant locations. Preserve
  // bodies, ranges, truncation flags, errors and any unfamiliar fields.
  if (related?.length) result.related = related;
  if (result.entity) {
    result.entity = {...result.entity};
    if (result.entity.file === result.file) delete result.entity.file;
    if (result.entity.parent_name === null) delete result.entity.parent_name;
  }
  return result;
}
