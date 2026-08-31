import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { startServersAndRegisterTools } from "../../extensions/pi-sem.ts";
import type { PiSemConfig } from "../../src/config/types.ts";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

// Finding #21/#22 ("fix now" item 1, folded into the same fix as
// #7): the tool lockdown must be fail-CLOSED. Even when every configured
// MCP server fails -- including a defect that makes registerServerTools
// itself throw, simulating a FUTURE regression of the #7 fix -- pi.setActiveTools
// must still be called, with exactly activeBuiltins + native tools, never
// left uncalled (which would leave pi's full default toolset active) and
// never silently expanded.

const __dirname = dirname(fileURLToPath(import.meta.url));
const CRASH_ON_LIST = join(__dirname, "fixtures", "fake-mcp-server-crash-on-tools-list.mjs");
const FAKE_MCP_SERVER = join(__dirname, "fixtures", "fake-mcp-server.mjs");

function makeFakePi() {
  const registeredToolNames: string[] = [];
  const activeToolsCalls: string[][] = [];
  const notices: { message: string; level?: string }[] = [];
  const pi = {
    registerTool: (def: { name: string }) => {
      registeredToolNames.push(def.name);
    },
    registerCommand: () => {},
    on: () => {},
    setActiveTools: (names: string[]) => {
      activeToolsCalls.push(names);
    },
    getActiveTools: () => activeToolsCalls.at(-1) ?? [],
    getAllTools: () => registeredToolNames.map((name) => ({ name })),
  };
  return { pi: pi as unknown as ExtensionAPI, registeredToolNames, activeToolsCalls, notices };
}

function makeFakeCtx(notices: { message: string; level?: string }[]) {
  return {
    ui: {
      notify: (message: string, level?: string) => {
        notices.push({ message, level });
      },
    },
    model: undefined,
    cwd: process.cwd(),
  } as unknown as ExtensionContext;
}

test("every server failing to start still reaches setActiveTools with activeBuiltins + native tools only", async () => {
  const { pi, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [
      { id: "sem", command: "/no/such/binary-sem", tools: {} },
      { id: "weave", command: "/no/such/binary-weave", tools: {} },
    ],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  // This test's premise is that both commands are unstartable -- a
  // PI_SEM_<ID>_MCP_BIN override in the invoking shell (legitimately set
  // to run the live weave-mcp tests) would rescue them and invert the
  // premise, so neutralize the overrides for this test's scope.
  const savedOverrides = { sem: process.env.PI_SEM_SEM_MCP_BIN, weave: process.env.PI_SEM_WEAVE_MCP_BIN };
  delete process.env.PI_SEM_SEM_MCP_BIN;
  delete process.env.PI_SEM_WEAVE_MCP_BIN;

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.equal(activeToolsCalls.length, 1, "setActiveTools should be called exactly once");
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "weave_edit", "sem_outline", "sem_read", "sem_find", "sem_grep", "sem_callers", "sem_graph", "sem_path", "sem_hotspots", "sem_cochange"]);
    assert.deepEqual(outcome.activeTools, ["bash", "write", "weave_edit", "sem_outline", "sem_read", "sem_find", "sem_grep", "sem_callers", "sem_graph", "sem_path", "sem_hotspots", "sem_cochange"]);
    assert.equal(outcome.statuses.length, 2);
    assert.ok(outcome.statuses.every((s) => s.startError));
    assert.ok(notices.length >= 2, "should have notified about each failed server");
  } finally {
    if (savedOverrides.sem !== undefined) process.env.PI_SEM_SEM_MCP_BIN = savedOverrides.sem;
    if (savedOverrides.weave !== undefined) process.env.PI_SEM_WEAVE_MCP_BIN = savedOverrides.weave;
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("a server whose listTools() crashes (real process, real crash) still reaches setActiveTools with the fail-closed set, and its client is stopped (no zombie)", async () => {
  const { pi, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [{ id: "crash-on-tools-list", command: process.execPath, args: [CRASH_ON_LIST], tools: {} }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "weave_edit", "sem_outline", "sem_read", "sem_find", "sem_grep", "sem_callers", "sem_graph", "sem_path", "sem_hotspots", "sem_cochange"]);
    assert.equal(outcome.statuses[0]?.startError !== undefined, true);
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("vacuous-fixture audit follow-up: a server that ACTUALLY REGISTERS a real tool surfaces it in the tools-mode active set, alongside the native tools -- not just when every server fails", async () => {
  // Every OTHER test in this file (and, before 83ea54f, every code-mode
  // test in extension-code-mode-fail-closed.test.ts) uses `tools: {}`
  // against a server that never starts or never answers tools/list -- all
  // are legitimately testing FAILURE resilience, where the allowlist's
  // content is irrelevant. But that meant this file's own claim ("the
  // active set is activeBuiltins + whatever registered") had never once
  // been exercised through the FULL startServersAndRegisterTools ->
  // computeActiveTools -> pi.setActiveTools pipeline with a server that
  // actually succeeds and actually registers something -- only at the
  // pure-function level (tool-policy.test.ts) and, now, at the
  // registerServerTools-unit level (register.test.ts). This closes that
  // integration-level gap using the same real fake-mcp-server.mjs fixture.
  const { pi, registeredToolNames, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [{ id: "weave", command: process.execPath, args: [FAKE_MCP_SERVER], tools: { echo: true } }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  // Same env-override neutralization as the all-fail test above: a
  // PI_SEM_WEAVE_MCP_BIN in the invoking shell would swap the fake fixture
  // for a real server that has no "echo" tool.
  const savedOverride = process.env.PI_SEM_WEAVE_MCP_BIN;
  delete process.env.PI_SEM_WEAVE_MCP_BIN;

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.ok(registeredToolNames.includes("echo"), "the allowlisted bridged tool must actually be registered in tools mode");
    assert.equal(outcome.statuses[0]?.startError, undefined);
    assert.deepEqual(outcome.statuses[0]?.registeredToolNames, ["echo"]);

    // Order matters (tool-policy.ts's documented contract): activeBuiltins,
    // then registered server tools, then the native extras.
    assert.deepEqual(activeToolsCalls[0], [
      "bash",
      "write",
      "echo",
      "weave_edit",
      "sem_outline",
      "sem_read",
      "sem_find",
      "sem_grep",
      "sem_callers",
      "sem_graph",
      "sem_path",
      "sem_hotspots",
      "sem_cochange",
    ]);
    assert.deepEqual(outcome.activeTools, activeToolsCalls[0]);
  } finally {
    if (savedOverride !== undefined) process.env.PI_SEM_WEAVE_MCP_BIN = savedOverride;
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("defense in depth: even if registerServerTools itself throws (simulating a future regression), setActiveTools is still called with the fail-closed set", async () => {
  const { pi, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  // A server config whose `tools` is intentionally malformed in a way that
  // would only matter if some future code path started trusting it without
  // validation; the real defense here is structural (the loop's own
  // try/catch), so this test exercises it via a command that spawn() itself
  // rejects synchronously-ish (a path with a null byte is guaranteed invalid
  // cross-platform and throws before any child process exists).
  const config: PiSemConfig = {
    servers: [{ id: "bad", command: "not-a-real-binary\0", tools: {} }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.equal(activeToolsCalls.length, 1);
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "weave_edit", "sem_outline", "sem_read", "sem_find", "sem_grep", "sem_callers", "sem_graph", "sem_path", "sem_hotspots", "sem_cochange"]);
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});
