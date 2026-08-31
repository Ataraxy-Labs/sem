#!/usr/bin/env node
// One agent of the cross-process coordination witness: runs a single
// performWeaveEdit with its own Coordinator (its own weave-mcp child
// process) and prints the outcome as JSON on stdout. Two of these run as
// separate OS processes against the same repo -- the only thing they
// share is the repo directory (disk + .weave CRDT state), exactly like
// two real agents.
//
// argv[2] is a JSON blob: { cwd, agentId, weaveMcp, semBin, params } for one
// edit (prints one outcome object), or { ..., paramsList: [...] } for a
// SEQUENCE of edits run back to back in this one process (prints an array of
// outcomes, one per edit). The sequence form is what the write-window race
// test needs: a single edit per process cannot free-run two agents against
// each other for long enough to hit the gate's pre-write window.
import { performWeaveEdit } from "../../../src/tools/weave-edit.ts";
import { Coordinator } from "../../../src/tools/internal/weave-coordination.ts";

const spec = JSON.parse(process.argv[2]);
const coordinator = new Coordinator({ command: spec.weaveMcp, cwd: spec.cwd, agentId: spec.agentId });
try {
  const deps = { cwd: spec.cwd, semBin: spec.semBin ?? "sem", coordinator, signal: undefined };
  if (spec.paramsList) {
    const outcomes = [];
    for (const params of spec.paramsList) outcomes.push(await performWeaveEdit(params, deps));
    process.stdout.write(JSON.stringify(outcomes));
  } else {
    process.stdout.write(JSON.stringify(await performWeaveEdit(spec.params, deps)));
  }
} catch (err) {
  process.stdout.write(JSON.stringify({ isError: true, text: `driver threw: ${err instanceof Error ? err.message : String(err)}`, details: {} }));
  process.exitCode = 1;
} finally {
  await coordinator.stop();
}
