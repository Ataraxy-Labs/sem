import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { performSemGraph, type SemGraphParams } from "../../src/tools/sem-graph.ts";
import { MAX_GRAPH_NODES } from "../../src/tools/internal/graph.ts";

function run(params: SemGraphParams, cwd: string) {
  return performSemGraph(params, { cwd, semBin: "sem" });
}

/**
 * PASS-4 FINDING (category a: "300-node/150-line caps honest").
 *
 * `computeGraph` (src/tools/internal/graph.ts) reports:
 *
 *   truncated: nodeIds.size >= MAX_GRAPH_NODES
 *
 * `bfsNeighborhood` stops ADDING new nodes once `visited.size` reaches
 * MAX_GRAPH_NODES (300) -- correct, that's the cap doing its job. But
 * `computeGraph`'s own truncation SIGNAL conflates two different things
 * that happen to produce the identical node count:
 *
 *   1. the BFS was cut off mid-exploration with more reachable nodes left
 *      unvisited (genuinely truncated -- the `>=` is meant to catch this)
 *   2. the neighborhood's TRUE size, with nothing left to explore, happens
 *      to be exactly 300 nodes (not truncated at all -- there was nothing
 *      more to find, the cap just happened to land exactly on the true
 *      answer)
 *
 * `visited.size >= MAX_GRAPH_NODES` cannot distinguish these two cases --
 * it only knows the final count, not whether the BFS frontier still had
 * unexplored candidates when it stopped. Case 2 is a real (if narrow)
 * false positive: the model is told "truncated at the node cap" for a
 * result that is, in fact, the complete and exact neighborhood.
 *
 * Verified end-to-end (not merely by code trace): a synthetic "star" file
 * of one `hub()` function directly calling 299 independent `leafN()`
 * functions (none of which call anything else) gives `sem graph --json`
 * exactly 300 entities / 299 edges. `sem_graph` on seed="hub",
 * direction="out" reports node_count=300, truncated=true -- and re-running
 * with hops=10 (vastly more than needed) returns the IDENTICAL 300-node
 * set, proving nothing was actually cut off; the neighborhood is genuinely,
 * completely exactly 300 nodes. `truncated: true` is simply false here.
 *
 * Same shape as an earlier (pass-2) `sem_outline` truncated-flag off-by-one
 * finding -- a boundary-count coincidence misreported as truncation.
 *
 * Fix sketch: `bfsNeighborhood` already knows, at the moment it stops
 * adding nodes, whether the frontier still had unvisited candidates
 * waiting (the `visited.size < MAX_GRAPH_NODES` guard inside the loop is
 * exactly the check that would tell it) -- return that boolean explicitly
 * (e.g. `{ nodeIds, edges, cappedMidExploration }`) instead of letting
 * `computeGraph` reconstruct a truncation signal after the fact purely
 * from the final size, which can't tell "stopped because full" from
 * "stopped because capped."
 */
test("sem_graph reports truncated=true for a neighborhood that is genuinely, completely exactly MAX_GRAPH_NODES large -- a false positive", async () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-graph-off-by-one-"));
  try {
    const leafCount = MAX_GRAPH_NODES - 1; // hub + leafCount leaves = exactly MAX_GRAPH_NODES nodes
    let src = "";
    for (let i = 1; i <= leafCount; i++) src += `function leaf${i}(): number { return ${i}; }\n`;
    src += "export function hub(): number {\n";
    for (let i = 1; i <= leafCount; i++) src += `  leaf${i}();\n`;
    src += "  return 0;\n}\n";
    writeFileSync(join(dir, "star.ts"), src, "utf8");

    const atExactCap = await run({ seed: "hub", direction: "out", hops: 1 }, dir);
    assert.equal(atExactCap.isError, false, atExactCap.text);
    assert.equal(atExactCap.details.node_count, MAX_GRAPH_NODES);

    // Vastly more hops than needed to reach every leaf (all direct children
    // of hub, none of which call anything else) returns the SAME node
    // count -- proof the neighborhood is naturally, completely this size,
    // not artificially cut off by the cap.
    const withMoreHops = await run({ seed: "hub", direction: "out", hops: 10 }, dir);
    assert.equal(withMoreHops.details.node_count, MAX_GRAPH_NODES);

    assert.equal(
      atExactCap.details.truncated,
      false,
      `truncated should be false -- the neighborhood is exactly ${MAX_GRAPH_NODES} nodes with nothing left to explore ` +
        `(confirmed by hops=10 returning the identical node count), not a BFS that was cut off mid-exploration. Got: ${atExactCap.text}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
