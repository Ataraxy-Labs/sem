import { test } from "node:test";
import assert from "node:assert/strict";
import { verifyEdit } from "../../src/tools/internal/verify.ts";
import type { Entity } from "../../src/tools/internal/entities.ts";

function entity(name: string, start: number, end: number): Entity {
  return {
    name,
    type: "function",
    start_line: start,
    end_line: end,
    start_byte: start,
    end_byte: end,
    parent_id: null,
    parentName: null,
    byteRangeReliable: true,
  };
}

test("a vanished neighbor is inconclusive, not grounds to reject a valid edit", () => {
  const target = entity("target", 1, 3);
  const neighbor = entity("neighbor", 5, 7);

  const result = verifyEdit([target, neighbor], [target], target, "replace");

  assert.equal(result.ok, false);
  assert.equal(result.conclusive, false);
  assert.match(result.reason ?? "", /sem\.check/);
});

test("a delete that leaves its target extractable conclusively fails", () => {
  const target = entity("target", 1, 3);

  assert.deepEqual(verifyEdit([target], [target], target, "delete"), {
    ok: false,
    conclusive: true,
    reason: '"target" is still extractable after a delete.',
  });
});

test("stable extraction conclusively verifies an edit", () => {
  const target = entity("target", 1, 3);
  const neighbor = entity("neighbor", 5, 7);

  assert.deepEqual(verifyEdit([target, neighbor], [target, neighbor], target, "replace"), {
    ok: true,
    conclusive: true,
  });
});
