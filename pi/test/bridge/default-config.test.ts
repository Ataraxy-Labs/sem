import { test } from "node:test";
import assert from "node:assert/strict";
import { DEFAULT_CONFIG } from "../../src/config/default.ts";
import { fromAllowlistFile } from "../../src/config/allowlist.ts";

// DEFAULT_CONFIG is derived from src/config/allowlist.json, the curated,
// LLM-facing tool inventory. These tests pin the conversion from that
// keyed-object, always-explicit-override file shape
// into the runtime PiSemConfig array-of-{id,...} shape register.ts and
// mcp-client.ts are actually built against.

test("DEFAULT_CONFIG has one ServerConfig per allowlist.json server key, each carrying its id", () => {
  const ids = DEFAULT_CONFIG.servers.map((s) => s.id).sort();
  assert.deepEqual(ids, ["sem", "weave"]);
});

test("DEFAULT_CONFIG carries the curated renames from allowlist.json (not the raw MCP names)", () => {
  const weave = DEFAULT_CONFIG.servers.find((s) => s.id === "weave");
  assert.ok(weave, "weave server should be present");
  const impact = weave.tools.weave_impact_analysis;
  assert.ok(impact && impact !== true, "weave_impact_analysis should carry a curated override, not bare true");
  assert.equal(impact.name, "weave_impact");
  assert.ok(impact.description && impact.description.length > 0);
  assert.ok(impact.promptGuidelines && impact.promptGuidelines.length > 0);
});

test("DEFAULT_CONFIG does not carry tools dropped from the current allowlist curation (replaced by native tools or cut for token cost)", () => {
  const weave = DEFAULT_CONFIG.servers.find((s) => s.id === "weave");
  assert.ok(weave, "weave server should be present");
  // weave_get_dependents (-> weave_dependents) was dropped in favor of the
  // native sem_callers; weave_status/weave_claim_entity/weave_release_entity
  // were cut earlier to reduce per-turn tool-definition token cost.
  // Asserting absence, not just a stale presence pin, so a future curation
  // change can't silently re-add one of these without this test noticing.
  for (const droppedKey of ["weave_get_dependents", "weave_status", "weave_claim_entity", "weave_release_entity"]) {
    assert.equal(weave.tools[droppedKey], undefined, `${droppedKey} should not be present in the current allowlist curation`);
  }

  const sem = DEFAULT_CONFIG.servers.find((s) => s.id === "sem");
  assert.ok(sem, "sem server should be present");
  // sem_context/sem_entities were dropped in favor of the native
  // sem_outline/sem_read; sem_blame/sem_log were cut for token cost.
  for (const droppedKey of ["sem_context", "sem_entities", "sem_blame", "sem_log"]) {
    assert.equal(sem.tools[droppedKey], undefined, `${droppedKey} should not be present in the current allowlist curation`);
  }
});

test("DEFAULT_CONFIG.sessionPolicy.activeBuiltins comes from allowlist.json's top-level activeBuiltins", () => {
  assert.deepEqual(DEFAULT_CONFIG.sessionPolicy.activeBuiltins, ["bash", "write"]);
});

test("DEFAULT_CONFIG.systemPromptAddendum comes from allowlist.json's top-level field", () => {
  // Deliberately not pinned to exact wording -- L2 iterates on this text.
  // Just confirms the field made it through the conversion non-empty and
  // still mentions the core tools it's meant to steer the model toward.
  assert.ok(DEFAULT_CONFIG.systemPromptAddendum && DEFAULT_CONFIG.systemPromptAddendum.length > 0);
  assert.match(DEFAULT_CONFIG.systemPromptAddendum, /sem_outline/);
  assert.match(DEFAULT_CONFIG.systemPromptAddendum, /weave_edit/);
});

test("fromAllowlistFile converts a minimal keyed-object file into the array-of-{id} runtime shape", () => {
  const converted = fromAllowlistFile({
    servers: {
      sem: { command: "sem", args: ["mcp"], tools: { sem_context: { name: "sem_context" } } },
    },
    activeBuiltins: ["bash"],
    systemPromptAddendum: "test addendum",
  });

  assert.equal(converted.servers.length, 1);
  const [server] = converted.servers;
  assert.ok(server);
  assert.equal(server.id, "sem");
  assert.equal(server.command, "sem");
  assert.deepEqual(converted.sessionPolicy, { activeBuiltins: ["bash"] });
  assert.equal(converted.systemPromptAddendum, "test addendum");
});
