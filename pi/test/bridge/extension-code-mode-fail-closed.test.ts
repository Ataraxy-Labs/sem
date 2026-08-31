import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import type { PiSemConfig } from "../../src/config/types.ts";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { CODE_MODE_ADDENDUM } from "../../src/codemode/tool.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_MCP_SERVER = join(__dirname, "fixtures", "fake-mcp-server.mjs");

/**
 * pisem-codemode wired `PI_SEM_MODE=code` into extensions/pi-sem.ts
 * (3673f59): when set, `startServersAndRegisterTools` registers ONE tool
 * (`sem_code`) instead of the usual six natives, and the `setActiveTools`
 * extras array becomes `[SEM_CODE_TOOL_NAME]` instead of the six tool-name
 * constants. Team-lead's ask: prove the fail-closed invariant this whole
 * file exists to protect -- pi.setActiveTools always gets called, exactly
 * once, with a closed (non-empty, bounded) set -- still holds in code mode,
 * not just the default mode extension-fail-closed.test.ts already covers.
 *
 * `CODE_MODE` in extensions/pi-sem.ts is a module-level `const` read ONCE
 * at import time (`process.env.PI_SEM_MODE === "code"`), matching how a
 * real `pi` process would set it before launch, not mid-session. That means
 * this test MUST set the env var before extensions/pi-sem.ts is ever
 * imported in this process. node --test gives each test FILE its own
 * process/module registry (no cross-file global-state leakage -- verified
 * empirically against Node 26's test runner), so setting the env var here
 * and then using a dynamic import() (not hoisted, unlike a static import)
 * is sufficient; no subprocess spawn needed.
 */

const previousMode = process.env.PI_SEM_MODE;
process.env.PI_SEM_MODE = "code";
// PI_SEM_PURE=0: this file pins code mode's EXPLICIT OPT-OUT surface --
// [bash, write, sem_code] -- now that pure ([sem_code] alone) is the
// default. Every activeBuiltins assertion below is therefore also the
// standing proof that the opt-out restores the historical surface
// exactly; the pure default itself is pinned by
// extension-pure-mode.test.ts with the env var absent.
const previousPure = process.env.PI_SEM_PURE;
process.env.PI_SEM_PURE = "0";

const { startServersAndRegisterTools, default: piSemExtension } = await import("../../extensions/pi-sem.ts");

if (previousMode === undefined) delete process.env.PI_SEM_MODE;
else process.env.PI_SEM_MODE = previousMode;
if (previousPure === undefined) delete process.env.PI_SEM_PURE;
else process.env.PI_SEM_PURE = previousPure;

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

test("code mode registers exactly one tool (sem_code) and still reaches setActiveTools with a closed set, even when every server fails", async () => {
  const { pi, registeredToolNames, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [
      { id: "sem", command: "/no/such/binary-sem", tools: {} },
      { id: "weave", command: "/no/such/binary-weave", tools: {} },
    ],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.equal(activeToolsCalls.length, 1, "setActiveTools should be called exactly once");
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "sem_code"], "code mode's active set must be activeBuiltins + sem_code only, not the six per-lookup natives");
    assert.deepEqual(outcome.activeTools, ["bash", "write", "sem_code"]);

    // The six default-mode natives must NOT be registered in code mode --
    // proves the branch is exclusive, not additive.
    for (const defaultModeTool of [
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
    ]) {
      assert.ok(!registeredToolNames.includes(defaultModeTool), `${defaultModeTool} must not be registered in code mode`);
    }
    assert.ok(registeredToolNames.includes("sem_code"), "sem_code must be registered in code mode");
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("authority-on-failure audit: code mode stays exactly [bash, write, sem_code] under a maximally adversarial config (missing binary + a dead allowlist entry + a healthy-but-unallowlisted server, all at once)", async () => {
  const { pi, registeredToolNames, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [
      { id: "sem", command: "/no/such/binary-sem", tools: { sem_impact: true, a_dead_entry_that_does_not_exist: true } },
      // Healthy, real server exposing a real tool ("echo") that would
      // register in tools mode (see the sibling positive-case test above)
      // -- code mode must ignore it just as completely as the broken one.
      { id: "weave", command: process.execPath, args: [join(__dirname, "fixtures", "fake-mcp-server.mjs")], tools: { echo: true } },
    ],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.deepEqual(registeredToolNames, ["sem_code"], "code mode must register nothing but sem_code, no matter how the servers are configured");
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "sem_code"]);
    assert.deepEqual(outcome.statuses, [], "code mode never even starts the bridged servers -- confirmed by 83ea54f's fix, re-checked here under an adversarial config");
    assert.deepEqual(outcome.clients, []);
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("code mode: setActiveTools is never left uncalled, matching the default-mode fail-closed guarantee", async () => {
  const { pi, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  // A config whose command has a null byte -- spawn() rejects synchronously,
  // exercising the same defense-in-depth catch extension-fail-closed.test.ts
  // proves for default mode.
  const config: PiSemConfig = {
    servers: [{ id: "bad", command: "not-a-real-binary\0", tools: {} }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    assert.equal(activeToolsCalls.length, 1);
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "sem_code"]);
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

test("code mode: a bridged MCP server's allowlisted tool is never registered at all, and never rides along in setActiveTools (real fail-open, now closed)", async () => {
  // The two existing tests above use `tools: {}` for every server -- an
  // empty allowlist, which meant `registerServerTools` always returned
  // `registeredToolNames: []` and could never have caught this bug no
  // matter how badly startServersAndRegisterTools mishandled CODE_MODE.
  // This test allowlists a REAL tool ("echo", from the fake MCP server
  // fixture already used by mcp-client.test.ts) on a server literally
  // named "weave", the same shape the real allowlist.json config uses for
  // weave_impact/sem_impact/sem_diff -- so it actually exercises the path
  // a real-session audit found leaking: 54.4% of code-mode sessions
  // (100% of the SWE-bench-style task suite) actually called a bridged
  // tool.
  const { pi, registeredToolNames, activeToolsCalls, notices } = makeFakePi();
  const ctx = makeFakeCtx(notices);

  const config: PiSemConfig = {
    servers: [{ id: "weave", command: process.execPath, args: [FAKE_MCP_SERVER], tools: { echo: true } }],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };

  const outcome = await startServersAndRegisterTools(pi, ctx, config);
  try {
    // The bridged tool must never even be registered on `pi` in code mode
    // -- not merely excluded from the active set. Registering it and
    // relying on setActiveTools to hide it is exactly the design this bug
    // report says is unsafe (see extensions/pi-sem.ts's mechanism comment
    // above the skipped loop).
    assert.ok(!registeredToolNames.includes("echo"), "the bridged server's allowlisted tool must not be registered in code mode at all");
    assert.deepEqual(registeredToolNames, ["sem_code"], "code mode must register nothing but sem_code");

    assert.equal(activeToolsCalls.length, 1);
    assert.deepEqual(activeToolsCalls[0], ["bash", "write", "sem_code"], "the bridged tool must not ride along in the active-tools list either");
    assert.deepEqual(outcome.activeTools, ["bash", "write", "sem_code"]);

    // The server itself is never started in code mode either (no client to
    // stop, no wasted process) -- confirms this is a full skip, not a
    // register-then-hide.
    assert.deepEqual(outcome.clients, []);
    assert.deepEqual(outcome.statuses, []);
  } finally {
    await Promise.all(outcome.clients.map((c) => c.stop()));
  }
});

// --- Full-lifecycle harness (through the default piSemExtension export,
// not just startServersAndRegisterTools) for the three remaining review
// checkpoints team-lead asked to verify in code mode: the bash/write audit
// wrappers, before_agent_start's addendum exclusivity, and /pi-sem's mode
// line. Mirrors extension-write-audit.test.ts's harness shape (multi-
// handler-per-event Map -- see extension-config-load-fail-open.review.test.ts
// for why overwrite semantics are wrong here: registerSemCode registers its
// OWN before_agent_start/session_shutdown handlers alongside piSemExtension's
// own, and both must fire).

interface FakeRegisteredTool {
  name: string;
  execute: (toolCallId: string, params: Record<string, unknown>, signal: unknown, onUpdate: unknown, execCtx: unknown) => Promise<unknown>;
}

function makeFullHarness(cwd: string) {
  const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
  const registeredTools = new Map<string, FakeRegisteredTool>();
  const activeToolsCalls: string[][] = [];
  const notices: { message: string; level?: string }[] = [];
  let commandHandler: ((args: unknown, ctx: unknown) => Promise<void>) | undefined;

  const pi = {
    on: (event: string, handler: (event: unknown, ctx: unknown) => unknown) => {
      const existing = handlers.get(event) ?? [];
      existing.push(handler);
      handlers.set(event, existing);
    },
    registerTool: (def: FakeRegisteredTool) => {
      registeredTools.set(def.name, def);
    },
    registerCommand: (_name: string, def: { handler: (args: unknown, ctx: unknown) => Promise<void> }) => {
      commandHandler = def.handler;
    },
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
    cwd,
  } as unknown as ExtensionContext;

  return { pi, ctx, handlers, registeredTools, activeToolsCalls, notices, getCommandHandler: () => commandHandler };
}

async function bootFullSession(h: ReturnType<typeof makeFullHarness>) {
  piSemExtension(h.pi);
  for (const handler of h.handlers.get("session_start") ?? []) {
    await Promise.resolve(handler({ type: "session_start", reason: "startup" }, h.ctx)).catch(() => {});
  }
}

async function shutdownFullSession(h: ReturnType<typeof makeFullHarness>) {
  for (const handler of h.handlers.get("session_shutdown") ?? []) {
    await Promise.resolve(handler({ type: "session_shutdown", reason: "quit" }, h.ctx)).catch(() => {});
  }
}

test("code mode: bash and write are still registered and audited (the policy wrapper is unaffected by the tool-set switch)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-codemode-audit-"));
  const configPath = join(dir, "config.json");
  writeFileSync(
    configPath,
    JSON.stringify({ servers: [], sessionPolicy: { activeBuiltins: ["bash", "write"] } } satisfies PiSemConfig),
    "utf8",
  );
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = configPath;
  const h = makeFullHarness(dir);

  try {
    await bootFullSession(h);
    assert.ok(h.registeredTools.has("bash"), "bash must still be registered in code mode");
    assert.ok(h.registeredTools.has("write"), "write must still be registered in code mode");
    assert.ok(h.registeredTools.has("sem_code"), "sem_code must be registered in code mode");
    assert.deepEqual(h.activeToolsCalls[0], ["bash", "write", "sem_code"]);

    // Audit still runs, not just registration -- a strict-mode read command
    // should still be refused through the wrapped bash tool in code mode.
    const previousStrict = process.env.PI_SEM_STRICT;
    process.env.PI_SEM_STRICT = "1";
    try {
      const bashTool = h.registeredTools.get("bash");
      assert.ok(bashTool);
      await assert.rejects(() => bashTool.execute("call-1", { command: "cat secret.txt" }, undefined, () => {}, { cwd: dir }));
    } finally {
      if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
      else process.env.PI_SEM_STRICT = previousStrict;
    }
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownFullSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("code mode: before_agent_start injects only the code-mode addendum, not the tools-mode loop addendum", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-codemode-prompt-"));
  const configPath = join(dir, "config.json");
  const toolsModeFingerprint = "TOOLS-MODE-LOOP-FINGERPRINT: outline -> choose -> read -> edit -> check impact";
  writeFileSync(
    configPath,
    JSON.stringify({
      servers: [],
      sessionPolicy: { activeBuiltins: ["bash", "write"] },
      systemPromptAddendum: toolsModeFingerprint,
    } satisfies PiSemConfig),
    "utf8",
  );
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = configPath;
  const h = makeFullHarness(dir);

  try {
    await bootFullSession(h);

    // Real pi's ExtensionRunner threads each before_agent_start handler's
    // systemPrompt output into the next handler's input (verified against
    // runner.js) -- reproduce that chaining here.
    let currentSystemPrompt = "BASE SYSTEM PROMPT";
    for (const handler of h.handlers.get("before_agent_start") ?? []) {
      const result = (await handler({ type: "before_agent_start", systemPrompt: currentSystemPrompt }, h.ctx)) as { systemPrompt?: string } | undefined;
      if (result?.systemPrompt !== undefined) currentSystemPrompt = result.systemPrompt;
    }

    assert.ok(currentSystemPrompt.includes(CODE_MODE_ADDENDUM), "code mode's own addendum must be present");
    assert.ok(
      !currentSystemPrompt.includes(toolsModeFingerprint),
      `the tools-mode addendum must NOT leak into code mode's system prompt, got:\n${currentSystemPrompt}`,
    );
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownFullSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("code mode: /pi-sem status reports the active mode", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-codemode-status-"));
  const configPath = join(dir, "config.json");
  writeFileSync(
    configPath,
    JSON.stringify({ servers: [], sessionPolicy: { activeBuiltins: ["bash", "write"] } } satisfies PiSemConfig),
    "utf8",
  );
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = configPath;
  const h = makeFullHarness(dir);

  try {
    await bootFullSession(h);
    const handler = h.getCommandHandler();
    assert.ok(handler, "pi-sem must register the /pi-sem command");
    await handler({}, h.ctx);

    assert.equal(h.notices.length, 1);
    assert.match(h.notices[0]?.message ?? "", /mode: code \(PI_SEM_MODE=code\)/);
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownFullSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});
