// Literal, explicitly scoped transformation; not a semantic rename.
// Expand server-side so the model need not repeat source or edit instructions.
export function repeatedEdits(exact, revision, {old, new: replacement, targets, allow_signature_change = false}) {
  if (typeof old !== 'string' || !old || typeof replacement !== 'string' || old === replacement ||
      typeof allow_signature_change !== 'boolean' || !Array.isArray(targets) || !targets.length || targets.length > 64) {
    throw new Error('INVALID_TRANSFORM');
  }
  const snapshot = exact.get(revision), seen = new Set(), files = new Set();
  const edits = [], changed = [];
  let bytes = 0;
  for (const target of targets) {
    if (!target || typeof target.id !== 'string' || !Number.isSafeInteger(target.count) || target.count < 1 ||
        Object.keys(target).some(key => !['id', 'count'].includes(key)) || seen.has(target.id)) {
      throw new Error('INVALID_TRANSFORM_TARGET');
    }
    seen.add(target.id);
    const entity = snapshot.byId.get(target.id);
    if (!entity) throw new Error('UNKNOWN_ENTITY');
    // Retain the existing exact-apply capability boundary, before any writes.
    if (files.has(entity.file)) throw new Error('MULTIPLE_TARGETS_PER_FILE_NOT_SUPPORTED');
    files.add(entity.file);
    const source = exact.read(revision, target.id).content;
    const parts = source.split(old), count = parts.length - 1;
    if (count !== target.count) throw new Error(`MATCH_COUNT_MISMATCH: ${target.id}; expected ${target.count}, found ${count}`);
    const size = Buffer.byteLength(source) + count * (Buffer.byteLength(replacement) - Buffer.byteLength(old));
    bytes += size;
    if (bytes > exact.maxBytes) throw new Error('RESULT_TOO_LARGE');
    edits.push({id: target.id, content: parts.join(replacement), allow_signature_change});
    changed.push({id: target.id, file: entity.file, replacements: count});
  }
  return {edits, summary: {targets: changed, replacements: changed.reduce((n, t) => n + t.replacements, 0),
    scope: 'explicit_entities_literal_nonoverlapping_matches', semantic_rename: false}};
}
