import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { registerServerTools } from "../../src/bridge/register.ts";
import { startServersAndRegisterTools } from "../../extensions/pi-sem.ts";
import type { PiSemConfig, ServerConfig } from "../../src/config/types.ts";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * team-lead's "authority on failure" audit (follow-up to 83ea54f and
 * f59b60a): for every failure mode along the bridge/extension startup and
 * runtime path, does the session end up with authority <= a healthy
 * session, or > (a bug of the same family as the last two fixes)? This
 * file covers every mode NOT already directly exercised elsewhere. Modes
 * covered by EXISTING tests are cited here rather than duplicated:
 *
 *  - sem/weave-mcp binary entirely missing (both servers) -- already
 *    proven fail-closed by extension-fail-closed.test.ts's "every server
 *    failing to start still reaches setActiveTools with activeBuiltins +
 *    native tools only".
 *  - a server whose listTools() CRASHES (process exits) -- already proven
 *    captured-not-thrown by register-tools-listtools-escape.review.test.ts
 *    and extension-fail-closed.test.ts's crash-on-tools-list test.
 *  - mid-session MCP client crash + one-allowed-restart + dead-after-
 *    second-crash -- already proven at the McpClient layer by
 *    mcp-client.test.ts's two crash tests (no new surface appears on
 *    respawn: the SAME configured command is re-spawned, and the already-
 *    registered tool closures just proxy through whatever process is
 *    currently backing them -- allowlist filtering happened once, at
 *    registration).
 *  - a server's real tool that IS allowlisted correctly registering,
 *    through the full extension path -- extension-fail-closed.test.ts's
 *    success-path sibling test (added for the vacuous-fixture audit).
 *  - code mode never touching config.servers at all when every server is
 *    broken -- extension-code-mode-fail-closed.test.ts's two failure-path
 *    tests (bogus binaries, null-byte command) already prove the code-mode
 *    active set stays exactly ["bash","write","sem_code"] regardless.
 *  - PI_SEM_STRICT=1 refusal still applying in code mode -- extension-
 *    code-mode-fail-closed.test.ts's "bash and write are still registered
 *    and audited" test.
 *  - PI_SEM_STRICT=1's own refusal logic -- write-audit.test.ts,
 *    bash-audit.test.ts (orthogonal to registration/authority entirely:
 *    it reads process.env.PI_SEM_STRICT fresh inside execute(), on every
 *    call, independent of config/servers/loadConfig -- it can only ever
 *    RESTRICT further, never grant more, so it cannot turn any mode below
 *    into a `>` on its own).
 *
 * New modes this file adds direct, non-degenerate evidence for:
 *  - a dead allowlist entry (a config key naming a tool the server never
 *    actually exposes)
 *  - a server's real tool that is NOT allowlisted, confirmed through the
 *    FULL startServersAndRegisterTools path (team-lead's explicit ask --
 *    register.test.ts only proved this at the registerServerTools-unit
 *    level)
 *  - tools/list hanging/timing out (as opposed to crashing) -- a new fixture
 *    mode (FAKE_MCP_HANG_TOOLS_LIST), proving registerServerTools's
 *    listTools() call is timeout-bounded, not an indefinite hang, and that
 *    the fail-closed setActiveTools call is still reached
 *  - config referencing an unknown/missing server id (the weaveServer
 *    lookup by id="weave" in startServersAndRegisterTools)
 *  - PI_SEM_STRICT=1 combined with a config that fails to load entirely
 *    (FAIL_CLOSED_CONFIG), through the full piSemExtension lifecycle
 *  - code mode under a maximally adversarial config (missing binaries +
 *    a dead entry + a healthy-but-unallowlisted server, all at once)
 */

const __dirname = dirname(fileURLToPath(import.meta.url));
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
    ui: { notify: (message: string, level?: string) => notices.push({ message, level }) },
    model: undefined,
    cwd: process.cwd(),
  } as unknown as ExtensionContext;
}

const NATIVE_TOOLS_MODE_SET = [
  "bash",
  "write",
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
];

// --- Mode: dead allowlist entry -------------------------------------------

test("authority-on-failure: a dead allowlist entry (names a tool the server never exposes) registers nothing and never throws", async () => {
  const config: ServerConfig = {
    id: "weave",
    command: process.execPath,
    args: [FAKE_MCP_SERVER],
    // The fake server only ever exposes "echo" -- this key names a tool
    // that provably does not exist on this server.
    tools: { this_tool_does_not_exist_on_the_server: true },
  };
  const fakePi = { registerTool: () => {} } as unknown as ExtensionAPI;
  const outcome = await registerServerTools(fakePi, config);
  try {
    assert.equal(outcome.result.startError, undefined, "the server itself is healthy");
    assert.deepEqual(outcome.result.registeredToolNames, [], "a dead allowlist entry must register nothing -- authority is a strict subset of a correct allowlist, never more");
  } finally {
    await outcome.client.stop();
  }
});

// --- Mode: server exposes a tool that is NOT in the allowlist, through the FULL extension path ---

test("authority-on-failure: a healthy server's real tool that is NOT allowlisted never reaches setActiveTools, through the full startServersAndRegisterTools path (not just the registerServerTools unit)", async () => {
  const { pi, registeredToolNames, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    // The fake server DOES expose "echo" -- but this config's allowlist is
    // empty, so it must never surface. This is the negative counterpart of
    // extension-fail-closed.test.ts's success-path sibling (which proves
    // an ALLOWLISTED tool DOES register) -- team-lead's explicit ask was to
    // confirm the exclusion side holds at this same integration level too,
    // not just at register.test.ts's registerServerTools-unit level.
    servers: [{ id: "weave", command: process.execPath, args: [FAKE_MCP_SERVER], tools: {} }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.equal(outcome.statuses[0]?.startError, undefined, "the server itself is healthy");
    assert.deepEqual(outcome.statuses[0]?.registeredToolNames, []);
    assert.ok(!registeredToolNames.includes("echo"), "echo must not be registered on pi at all");
    assert.deepEqual(activeToolsCalls[0], NATIVE_TOOLS_MODE_SET, "the active set must be exactly native tools + activeBuiltins -- no bridged authority leaked in");
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

// --- Mode: tools/list hangs/times out (as opposed to crashing) ------------

test("authority-on-failure: a server whose tools/list HANGS (never crashes, never responds) is captured as a timeout, never hangs the session, and never registers anything", async () => {
  const config: ServerConfig = {
    id: "weave",
    command: process.execPath,
    args: [FAKE_MCP_SERVER],
    env: { FAKE_MCP_HANG_TOOLS_LIST: "1" },
    requestTimeoutMs: 300, // short, so this test doesn't wait on the 30s default
    tools: { echo: true },
  };
  const fakePi = { registerTool: () => {} } as unknown as ExtensionAPI;

  const start = Date.now();
  const outcome = await registerServerTools(fakePi, config);
  const elapsedMs = Date.now() - start;
  try {
    assert.ok(elapsedMs < 5000, `registerServerTools must resolve within the configured requestTimeoutMs, not hang -- took ${elapsedMs}ms`);
    assert.ok(outcome.result.startError, "a listTools() timeout must be captured as startError, mirroring the crash path");
    assert.match(outcome.result.startError?.message ?? "", /timed out/);
    assert.deepEqual(outcome.result.registeredToolNames, []);
  } finally {
    await outcome.client.stop();
  }
});

test("authority-on-failure: a hanging tools/list still lets the full session reach setActiveTools with the fail-closed set, bounded by requestTimeoutMs, not an indefinite hang", async () => {
  const { pi, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [{ id: "weave", command: process.execPath, args: [FAKE_MCP_SERVER], env: { FAKE_MCP_HANG_TOOLS_LIST: "1" }, requestTimeoutMs: 300, tools: { echo: true } }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const start = Date.now();
  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  const elapsedMs = Date.now() - start;
  try {
    assert.ok(elapsedMs < 5000, `the whole session_start-equivalent path must not hang indefinitely on a stuck server -- took ${elapsedMs}ms`);
    assert.equal(activeToolsCalls.length, 1, "setActiveTools must still be reached exactly once");
    assert.deepEqual(activeToolsCalls[0], NATIVE_TOOLS_MODE_SET, "a hung server must never contribute authority -- same fail-closed set as any other failed server");
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

// --- Mode: config parses but references an unknown/missing server id -----

test("authority-on-failure: a config with no entry for id \"weave\" leaves weave_edit registered (PATH-fallback command), never crashes, and grants no extra authority", async () => {
  const { pi, registeredToolNames, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  // config.servers omits the "weave" id entirely -- extensions/pi-sem.ts's
  // `config.servers.find((s) => s.id === "weave")` lookup returns
  // undefined, so weave_edit/registerSemCode fall back to their own
  // PATH-resolved default command rather than the (missing) configured
  // one. Confirms this degrades gracefully -- same tool name, no throw,
  // no additional surface -- rather than granting anything unexpected.
  const config: PiSemConfig = {
    servers: [{ id: "sem", command: "/no/such/binary-sem", tools: {} }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.ok(registeredToolNames.includes("weave_edit"), "weave_edit must still register even with no matching server id in config");
    assert.deepEqual(activeToolsCalls[0], NATIVE_TOOLS_MODE_SET, "no unexpected extra tool appears just because a server id went unmatched");
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

// --- Mode: PI_SEM_STRICT=1 combined with a config that fails to load entirely ---

test("authority-on-failure: PI_SEM_STRICT=1 still refuses a risky bash command even when PI_SEM_CONFIG itself failed to load (FAIL_CLOSED_CONFIG path), through the full piSemExtension lifecycle", async () => {
  const { mkdtempSync, rmSync, writeFileSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const piSemExtensionModule = await import("../../extensions/pi-sem.ts");
  const piSemExtension = piSemExtensionModule.default;

  const dir = mkdtempSync(join(tmpdir(), "pi-sem-strict-failclosed-"));
  const brokenConfigPath = join(dir, "broken.json");
  writeFileSync(brokenConfigPath, "{ not valid json", "utf8");

  const previousConfig = process.env.PI_SEM_CONFIG;
  const previousStrict = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_CONFIG = brokenConfigPath;
  process.env.PI_SEM_STRICT = "1";

  interface FakeRegisteredTool {
    name: string;
    execute: (toolCallId: string, params: Record<string, unknown>, signal: unknown, onUpdate: unknown, execCtx: unknown) => Promise<unknown>;
  }

  const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
  const registeredTools = new Map<string, FakeRegisteredTool>();
  const activeToolsCalls: string[][] = [];
  const notices: { message: string; level?: string }[] = [];

  const pi = {
    on: (event: string, handler: (event: unknown, ctx: unknown) => unknown) => {
      const existing = handlers.get(event) ?? [];
      existing.push(handler);
      handlers.set(event, existing);
    },
    registerTool: (def: FakeRegisteredTool) => {
      registeredTools.set(def.name, def);
    },
    registerCommand: () => {},
    setActiveTools: (names: string[]) => {
      activeToolsCalls.push(names);
    },
    getActiveTools: () => activeToolsCalls.at(-1) ?? [],
    getAllTools: () => [...registeredTools.keys()].map((name) => ({ name })),
    appendEntry: () => {},
  } as unknown as ExtensionAPI;

  const ctx = {
    ui: { notify: (message: string, level?: string) => notices.push({ message, level }) },
    model: undefined,
    cwd: dir,
  } as unknown as ExtensionContext;

  try {
    piSemExtension(pi);
    for (const handler of handlers.get("session_start") ?? []) {
      await Promise.resolve(handler({ type: "session_start", reason: "startup" }, ctx)).catch(() => {});
    }

    assert.deepEqual(activeToolsCalls[0], NATIVE_TOOLS_MODE_SET, "even a totally broken PI_SEM_CONFIG must still land on the fail-closed set");

    const bashTool = registeredTools.get("bash");
    assert.ok(bashTool, "the bash audit wrapper must still be registered when the config itself failed to load");
    await assert.rejects(
      () => bashTool.execute("call-1", { command: "cat secret.env" }, undefined, () => {}, { cwd: dir }),
      /pi-sem/,
      "PI_SEM_STRICT=1 must still refuse a risky bash command even under the fail-closed config fallback -- strict enforcement is independent of config load success",
    );
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = previousStrict;
    for (const handler of handlers.get("session_shutdown") ?? []) {
      await Promise.resolve(handler({ type: "session_shutdown", reason: "quit" }, ctx)).catch(() => {});
    }
    rmSync(dir, { recursive: true, force: true });
  }
});

// Mode "code mode under a maximally adversarial config" lives in
// extension-code-mode-fail-closed.test.ts instead of here: that file sets
// PI_SEM_MODE=code and dynamically imports extensions/pi-sem.ts BEFORE any
// other import of that module touches the process's module cache. This
// file already has a static top-level import of startServersAndRegisterTools
// (used by the tools-mode tests above) -- Node caches ES modules by
// specifier, so a later dynamic re-import of the SAME path in THIS file
// would just return the already-loaded (CODE_MODE=false) module instance,
// silently testing the wrong branch. Caught empirically: the first version
// of this test lived here and failed with the full tools-mode set instead
// of ["sem_code"], not because of a real bug but because of this exact
// caching hazard -- moved rather than worked around, to match the one
// pattern in this suite already proven safe for code-mode module state.
