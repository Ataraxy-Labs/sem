// Locators have a separate budget from source bodies. Never silently discard
// candidates merely because hydrating their bodies would be expensive.
export function matchInventory(group, hits, offset = 0, limit = 60) {
  const total = Number.isInteger(group.total) ? Math.max(group.total, hits.length) : hits.length;
  const page = hits.slice(offset, offset + limit);
  const availableMore = offset + page.length < hits.length;
  const upstreamTruncated = total > hits.length || group.truncated === true;
  return {
    pattern: group.pattern, total, returned: page.length,
    omitted: Math.max(0, total - page.length),
    complete: offset === 0 && !availableMore && !upstreamTruncated,
    next_offset: availableMore ? offset + page.length : null,
    upstream_truncated: upstreamTruncated,
    next_action: availableMore ? 'Repeat with match_offset=next_offset.' :
      upstreamTruncated ? 'Narrow path or pattern; upstream search was bounded.' : null,
    hits: page.map(({file, line, text}) => ({file, line, text})),
  };
}

// Cache only within one plan: edits between plans must never reuse stale names.
export function measuredLookup(lookup, timings) {
  const pending = new Map();
  return async name => {
    if (!pending.has(name)) {
      const started = performance.now();
      pending.set(name, Promise.resolve().then(() => lookup(name)).then(
        result => { timings.push({name, ms: performance.now()-started, hits: result.hits?.length ?? 0}); return result; },
        error => { timings.push({name, ms: performance.now()-started, error: String(error)}); throw error; },
      ));
    }
    return pending.get(name);
  };
}
