import { test } from "node:test";
import assert from "node:assert/strict";
import { computeActiveTools } from "../../src/bridge/tool-policy.ts";
import type { RegisterServerResult } from "../../src/bridge/register.ts";
import type { SessionPolicy } from "../../src/config/types.ts";

// Pins the fail-closed invariant from the ox review (finding #21/#22, "fix
// now" item 1): the active tool set is ALWAYS computed from
// sessionPolicy.activeBuiltins + whatever actually registered, even when
// every server failed -- never a silent fall-through to pi's full default
// toolset (read/edit/grep/find/ls). This is a pure function specifically so
// the invariant can be proven without spawning any process.

test("activeBuiltins + extras only, when every server registered zero tools (fail-closed)", () => {
  const sessionPolicy: SessionPolicy = { activeBuiltins: ["bash", "write"] };
  const results: RegisterServerResult[] = [
    { serverId: "sem", registeredToolNames: [], startError: new Error("boom") },
    { serverId: "weave", registeredToolNames: [], startError: new Error("boom2") },
  ];
  assert.deepEqual(computeActiveTools(sessionPolicy, results, ["weave_edit"]), ["bash", "write", "weave_edit"]);
});

test("includes tools that did register even when other servers failed", () => {
  const sessionPolicy: SessionPolicy = { activeBuiltins: ["bash", "write"] };
  const results: RegisterServerResult[] = [
    { serverId: "sem", registeredToolNames: ["sem_impact", "sem_diff"] },
    { serverId: "weave", registeredToolNames: [], startError: new Error("boom") },
  ];
  assert.deepEqual(computeActiveTools(sessionPolicy, results, ["weave_edit"]), [
    "bash",
    "write",
    "sem_impact",
    "sem_diff",
    "weave_edit",
  ]);
});

test("zero server results (e.g. an empty servers[] config) still returns activeBuiltins + extras, never empty/undefined", () => {
  const sessionPolicy: SessionPolicy = { activeBuiltins: ["bash", "write"] };
  assert.deepEqual(computeActiveTools(sessionPolicy, [], ["weave_edit"]), ["bash", "write", "weave_edit"]);
});

test("extras default to empty when omitted", () => {
  const sessionPolicy: SessionPolicy = { activeBuiltins: ["bash"] };
  assert.deepEqual(computeActiveTools(sessionPolicy, []), ["bash"]);
});

test("preserves order: activeBuiltins, then registered tools in server order, then extras", () => {
  const sessionPolicy: SessionPolicy = { activeBuiltins: ["bash", "write"] };
  const results: RegisterServerResult[] = [
    { serverId: "sem", registeredToolNames: ["sem_impact"] },
    { serverId: "weave", registeredToolNames: ["weave_status"] },
  ];
  assert.deepEqual(computeActiveTools(sessionPolicy, results, ["weave_edit"]), [
    "bash",
    "write",
    "sem_impact",
    "weave_status",
    "weave_edit",
  ]);
});
