import { test } from "node:test";
import assert from "node:assert/strict";
import { parseClaimLoser } from "../../../src/tools/internal/weave-coordination.ts";

/**
 * A lost claim is an MCP-level SUCCESS: weave-mcp's ClaimResult::AlreadyClaimed
 * is an Ok variant, so it arrives with isError:false and the outcome in the
 * JSON payload. Coordinator.claim() trusting isError alone reported ok:true
 * for a claim the agent never won. These pin the payload parsing both in the
 * nested shape weave-mcp actually emits and the un-nested defensive shape.
 */

test("nested {result:{AlreadyClaimed:{by}}} names the holder", () => {
  assert.equal(parseClaimLoser(JSON.stringify({ result: { AlreadyClaimed: { by: "agent-A" } } })), "agent-A");
});

test("un-nested {AlreadyClaimed:{by}} names the holder", () => {
  assert.equal(parseClaimLoser(JSON.stringify({ AlreadyClaimed: { by: "agent-B" } })), "agent-B");
});

test("AlreadyClaimed without a usable holder still reports a loss", () => {
  assert.equal(parseClaimLoser(JSON.stringify({ AlreadyClaimed: {} })), "another agent");
});

test("a won claim (no AlreadyClaimed anywhere) is not a loss", () => {
  assert.equal(parseClaimLoser(JSON.stringify({ result: "Claimed", entity_id: "e1" })), undefined);
});

test("non-JSON text is not a loss (older servers, prose responses)", () => {
  assert.equal(parseClaimLoser("claimed ok"), undefined);
});
