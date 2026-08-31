import { test } from "node:test";
import assert from "node:assert/strict";
import { tokenEstimate } from "../../src/tools/sem-outline.ts";
import type { Entity } from "../../src/tools/internal/entities.ts";

function entity(overrides: Partial<Entity>): Entity {
  return {
    name: "x",
    type: "function",
    start_line: 1,
    end_line: 1,
    parent_id: null,
    parentName: null,
    byteRangeReliable: false,
    ...overrides,
  };
}

// sem-outline's tokenEstimate() must derive its estimate from
// Entity.byteRangeReliable alone, not from any assumption about which sem
// version or entity kind produced it — whether non-code entities carry a
// trustworthy byte range at all is sem-version-dependent (confirmed
// empirically: sem 0.23.0 omits byte ranges for markdown/toml entities,
// 0.23.1+ includes one), so these feed captured shapes directly through
// the same function every outline row actually uses, instead of pinning a
// live `sem` binary's exact byte-counting behavior end-to-end.
test("tokenEstimate is byte-derived (~4 bytes/token) when byteRangeReliable is true — the sem >=0.23.1 shape, including for non-code entities", () => {
  const e = entity({ type: "section", start_line: 1, end_line: 3, start_byte: 0, end_byte: 40, byteRangeReliable: true });
  assert.equal(tokenEstimate(e), 10);
});

test("tokenEstimate falls back to a line-count heuristic (10 tokens/line) when byteRangeReliable is false — the sem <=0.23.0 shape for non-code entities", () => {
  const e = entity({ type: "heading", start_line: 5, end_line: 9, byteRangeReliable: false });
  assert.equal(tokenEstimate(e), 50); // 5 lines * 10
});

test("tokenEstimate never reports zero even for a tiny byte range", () => {
  const e = entity({ start_byte: 0, end_byte: 1, byteRangeReliable: true, start_line: 1, end_line: 1 });
  assert.equal(tokenEstimate(e), 1);
});

test("tokenEstimate is byte-derived for an ordinary code entity (function), same as any other byteRangeReliable=true entity", () => {
  const e = entity({ type: "function", start_line: 13, end_line: 15, start_byte: 0, end_byte: 52, byteRangeReliable: true });
  assert.equal(tokenEstimate(e), 13); // 52/4
});
