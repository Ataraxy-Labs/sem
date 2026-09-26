// Budgets here count UTF-16 code units, matching String.slice in the reader.
// Never advertise a sliced definition as complete: edit caches rely on this flag.
export function boundedSource(source, budget) {
  if (!Number.isInteger(budget) || budget < 1) throw new RangeError('budget must be a positive integer');
  const truncated = source.length > budget;
  let end = Math.min(source.length, budget);
  // Do not split an astral character between its surrogate code units.
  if (truncated && end > 0 && /[\uD800-\uDBFF]/.test(source[end - 1]) && /[\uDC00-\uDFFF]/.test(source[end])) end--;
  return {content: source.slice(0, end), truncated};
}
