// Join every worker before returning/throwing so callers may safely clean up
// temporary parser inputs. Output order is independent of completion order.
export async function parallelMap(items, concurrency, visit) {
  if (!Number.isSafeInteger(concurrency) || concurrency < 1 || concurrency > 16)
    throw Error('INVALID_PARSE_CONCURRENCY');
  const output = new Array(items.length);
  let cursor = 0, failure;
  await Promise.all(Array.from({length: Math.min(concurrency, items.length)}, async () => {
    while (!failure) {
      const index = cursor++;
      if (index >= items.length) return;
      try { output[index] = await visit(items[index], index); }
      catch (error) { failure ??= {error}; }
    }
  }));
  if (failure) throw failure.error;
  return output;
}
