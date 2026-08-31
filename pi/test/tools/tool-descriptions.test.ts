import { test } from "node:test";
import assert from "node:assert/strict";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { registerWeaveEdit } from "../../src/tools/weave-edit.ts";
import { registerSemOutline } from "../../src/tools/sem-outline.ts";
import { registerSemRead } from "../../src/tools/sem-read.ts";
import { registerSemFind } from "../../src/tools/sem-find.ts";
import { registerSemGrep } from "../../src/tools/sem-grep.ts";
import { registerSemCallers } from "../../src/tools/sem-callers.ts";
import { registerSemGraph } from "../../src/tools/sem-graph.ts";
import { registerSemPath } from "../../src/tools/sem-path.ts";
import { registerSemHotspots } from "../../src/tools/sem-hotspots.ts";
import { registerSemCochange } from "../../src/tools/sem-cochange.ts";

const MAX_DESCRIPTION_LENGTH = 160;

interface CapturedTool {
  name: string;
  description: string;
}

// Every request pays for every registered tool's description in the system
// prompt's "Available tools" section — eval data showed pi-sem costing far
// more tokens than vanilla pi per turn (ts-tokeff: 8k -> 102k), and fat tool
// metadata is part of that. Descriptions are capped here as a cheap,
// permanent guardrail against re-growing them.
function fakePi(): { pi: ExtensionAPI; captured: CapturedTool[] } {
  const captured: CapturedTool[] = [];
  const pi = {
    registerTool(def: { name: string; description: string }) {
      captured.push({ name: def.name, description: def.description });
    },
    on() {},
  } as unknown as ExtensionAPI;
  return { pi, captured };
}

test("every registered tool's description is at most 160 characters", () => {
  const registrars: Array<[string, (pi: ExtensionAPI) => void]> = [
    ["weave_edit", registerWeaveEdit],
    ["sem_outline", registerSemOutline],
    ["sem_read", registerSemRead],
    ["sem_find", registerSemFind],
    ["sem_grep", registerSemGrep],
    ["sem_callers", registerSemCallers],
    ["sem_graph", registerSemGraph],
    ["sem_path", registerSemPath],
    ["sem_hotspots", registerSemHotspots],
    ["sem_cochange", registerSemCochange],
  ];

  for (const [expectedName, register] of registrars) {
    const { pi, captured } = fakePi();
    register(pi);
    assert.equal(captured.length, 1, `${expectedName} should register exactly one tool`);
    const tool = captured[0]!;
    assert.equal(tool.name, expectedName);
    assert.ok(
      tool.description.length <= MAX_DESCRIPTION_LENGTH,
      `${expectedName}'s description is ${tool.description.length} chars, over the ${MAX_DESCRIPTION_LENGTH} cap: "${tool.description}"`,
    );
  }
});
