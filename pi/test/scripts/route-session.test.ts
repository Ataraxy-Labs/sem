import assert from "node:assert/strict";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const script = path.join(root, "scripts/route-session.mjs");

function route(input: string) {
  const result = spawnSync(process.execPath, ["--experimental-strip-types", script], {
    cwd: root,
    input,
    encoding: "utf8",
  });
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout);
}

test("route-session accepts a language-neutral JSON envelope", () => {
  const decision = route(JSON.stringify({
    task: "Rename this API and update all callers across the repository",
    files: ["src/api.ts", "src/client.ts"],
    index_warm: true,
  }));
  assert.equal(decision.protocol, "ataraxy.session-route.v1");
  assert.equal(decision.mode, "structural");
  assert.equal(decision.attach_structural_tools, true);
  assert.equal(decision.index, "warm");
  assert.equal(decision.fallback, "native");
});

test("route-session accepts raw task text for shell adapters", () => {
  const decision = route("Aggregate JSONL records into output.json");
  assert.equal(decision.mode, "native");
  assert.equal(decision.attach_structural_tools, false);
});

test("invalid protocol input fails open with a native decision", () => {
  const decision = route('{"files": [42]}');
  assert.equal(decision.mode, "native");
  assert.equal(decision.attach_structural_tools, false);
  assert.deepEqual(decision.signals, ["router-error"]);
});
