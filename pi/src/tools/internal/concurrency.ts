/**
 * OX-REVIEW-3 FINDING: sem_find's queries= and sem_grep's patterns= ran
 * every item through `Promise.all` unbounded — a caller passing a large
 * array fires that many `sem` child processes at once, with no ceiling on
 * either how many run or how many get accepted at all. Two independent
 * guards belong at the batch boundary: an honest cap on array length (mirror
 * sem_read's entities= "spent the whole budget, said so" cap — an item past
 * the cap is reported as skipped, not silently dropped or silently run
 * anyway) and a concurrency ceiling on how many run at once (so the cap
 * doesn't just move the same unbounded fan-out one level down).
 *
 * mapWithConcurrency is the second guard: a small, dependency-free bounded
 * `Promise.all` — no `p-limit`-style external package needed for "run up to
 * N of these Promise-returning calls at once, preserve input order in the
 * result." A worker-pool of `limit` (or `items.length`, whichever is
 * smaller) pulls the next index off a shared cursor as each of its slots
 * frees up, so a fast call doesn't sit idle waiting for a slow one ahead of
 * it in the array.
 */
export async function mapWithConcurrency<T, R>(items: readonly T[], limit: number, fn: (item: T, index: number) => Promise<R>): Promise<R[]> {
  const results: R[] = new Array(items.length);
  const poolSize = Math.max(1, Math.min(limit, items.length));
  let nextIndex = 0;

  async function worker(): Promise<void> {
    while (true) {
      const i = nextIndex++;
      if (i >= items.length) return;
      results[i] = await fn(items[i]!, i);
    }
  }

  await Promise.all(Array.from({ length: poolSize }, worker));
  return results;
}
