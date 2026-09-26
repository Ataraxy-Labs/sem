// Preserve lexical containment semantics, including coincident/crossing ranges.
// Only entities in the same file can be ancestors. Completed ranges leave the
// active set, so ordinary flat files take sorting + linear work, not N² scans.
export function qualifyEntities(entities) {
  const files = new Map();
  for (const entity of entities) {
    if (!files.has(entity.file)) files.set(entity.file, []);
    files.get(entity.file).push(entity);
  }
  for (const rows of files.values()) {
    const ordered = [...rows].sort((a, b) => a.start - b.start || b.end - a.end);
    let active = [];
    for (let i = 0; i < ordered.length;) {
      const first = ordered[i];
      let end = i + 1;
      while (end < ordered.length && ordered[end].start === first.start &&
             ordered[end].end === first.end) end++;
      active = active.filter(p => p.end >= first.start);
      const prefix = active.filter(p => p.end >= first.end).map(p => p.name);
      // Equal ranges are siblings, never parents of one another.
      for (let j = i; j < end; j++) {
        ordered[j].qualified_name = [...prefix, ordered[j].name].join('.');
      }
      for (let j = i; j < end; j++) active.push(ordered[j]);
      i = end;
    }
  }
}

export function indexEntities(entities) {
  const byId = new Map(), byName = new Map();
  for (const entity of entities) {
    byId.set(entity.id, entity);
    for (const name of new Set([entity.name, entity.qualified_name])) {
      if (!byName.has(name)) byName.set(name, []);
      byName.get(name).push(entity);
    }
  }
  return { byId, byName };
}

export function lookupEntities(snapshot, selector) {
  const candidates = selector.id
    ? (snapshot.byId.has(selector.id) ? [snapshot.byId.get(selector.id)] : [])
    : snapshot.byName.get(selector.name) ?? [];
  return candidates.filter(e => (!selector.file || e.file === selector.file) &&
    (!selector.type || e.type === selector.type));
}
