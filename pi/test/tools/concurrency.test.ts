import { test } from "node:test";
import assert from "node:assert/strict";
import { mapWithConcurrency } from "../../src/tools/internal/concurrency.ts";

function delay(ms: number): Promise<void> {
  return new Promise((resolvePromise) => setTimeout(resolvePromise, ms));
}

test("mapWithConcurrency preserves input order in the result regardless of completion order", async () => {
  // Item 0 finishes LAST (longest delay) and item 4 finishes FIRST, so a
  // naive "push results as they complete" implementation would scramble
  // the order -- the result array must still read [0, 1, 2, 3, 4].
  const items = [50, 40, 30, 20, 10];
  const results = await mapWithConcurrency(items, 5, async (ms, i) => {
    await delay(ms);
    return i;
  });
  assert.deepEqual(results, [0, 1, 2, 3, 4]);
});

test("mapWithConcurrency never runs more than `limit` calls at once", async () => {
  let active = 0;
  let maxActive = 0;
  const items = Array.from({ length: 12 }, (_, i) => i);

  await mapWithConcurrency(items, 3, async (i) => {
    active++;
    maxActive = Math.max(maxActive, active);
    await delay(5);
    active--;
    return i;
  });

  assert.ok(maxActive <= 3, `expected at most 3 concurrent calls, observed ${maxActive}`);
  assert.ok(maxActive > 1, `expected genuine concurrency (>1), observed ${maxActive} -- otherwise this isn't testing anything`);
});

test("mapWithConcurrency runs every item to completion even when limit exceeds the item count", async () => {
  const items = ["a", "b", "c"];
  const results = await mapWithConcurrency(items, 10, async (s) => s.toUpperCase());
  assert.deepEqual(results, ["A", "B", "C"]);
});

test("mapWithConcurrency on an empty array resolves to an empty array without calling fn", async () => {
  let calls = 0;
  const results = await mapWithConcurrency<number, number>([], 4, async (i) => {
    calls++;
    return i;
  });
  assert.deepEqual(results, []);
  assert.equal(calls, 0);
});

test("mapWithConcurrency propagates a rejection from any one call", async () => {
  const items = [1, 2, 3];
  await assert.rejects(
    mapWithConcurrency(items, 2, async (i) => {
      if (i === 2) throw new Error("boom");
      return i;
    }),
    /boom/,
  );
});
