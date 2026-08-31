import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import piSemExtension, { packageBypassWarning, BYPASS_TOOL_NAMES } from "../../extensions/pi-sem.ts";
import type { PiSemConfig } from "../../src/config/types.ts";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * A later review round found the old bypass warning was keyed on model
 * PROVIDER ("openai-codex"), asserted these tools were "invisible to
 * pi.registerTool/setActiveTools". Both were wrong. Verified against a real
 * pi session (probe extension + `pi -e ... --mode json -p "..."`, `~/.pi/
 * agent/settings.json`'s `packages` list): exec_command et al. ARE
 * registered via pi.registerTool (fully visible in pi.getAllTools(), with
 * an accurate sourceInfo), already present by the time session_start fires
 * for ANY extension (package registration happens at extension-load time,
 * before session lifecycle events), and their sourceInfo names the actual
 * cause -- `source: "npm:@howaboua/pi-codex-conversion"`, `origin:
 * "package"` -- independent of which provider/model is active. Provider was
 * neither necessary (a different provider with the package still installed
 * would go unflagged) nor sufficient (openai-codex without the package
 * would be wrongly flagged).
 *
 * These tests exercise packageBypassWarning directly (a fake pi.getAllTools()
 * returning ToolInfo-shaped objects with a real sourceInfo), the same shape
 * a real pi session's getAllTools() actually returns.
 */

function fakeToolInfo(name: string, source?: string) {
  return {
    name,
    description: "",
    parameters: undefined,
    promptGuidelines: undefined,
    sourceInfo: source === undefined ? undefined : { path: "/fake/path", source, scope: "user" as const, origin: "package" as const },
  };
}

function fakePiWithTools(tools: ReturnType<typeof fakeToolInfo>[]): ExtensionAPI {
  return { getAllTools: () => tools } as unknown as ExtensionAPI;
}

test("packageBypassWarning returns undefined when no bypass tool names are registered", () => {
  const pi = fakePiWithTools([fakeToolInfo("bash"), fakeToolInfo("write"), fakeToolInfo("sem_outline")]);
  assert.equal(packageBypassWarning(pi), undefined);
});

test("packageBypassWarning names the actual registering package, not a provider", () => {
  const pi = fakePiWithTools([
    fakeToolInfo("bash"),
    fakeToolInfo("exec_command", "npm:@howaboua/pi-codex-conversion"),
  ]);
  const warning = packageBypassWarning(pi);
  assert.ok(warning);
  assert.match(warning, /npm:@howaboua\/pi-codex-conversion/);
  assert.match(warning, /exec_command/);
});

test("packageBypassWarning covers every name in BYPASS_TOOL_NAMES individually", () => {
  for (const name of BYPASS_TOOL_NAMES) {
    const pi = fakePiWithTools([fakeToolInfo(name, "npm:@howaboua/pi-codex-conversion")]);
    const warning = packageBypassWarning(pi);
    assert.ok(warning, `expected a warning for bypass tool name "${name}"`);
    assert.match(warning, new RegExp(name));
  }
});

test("packageBypassWarning lists every distinct bypass tool name found, deduped, when several are registered from the same package", () => {
  const pi = fakePiWithTools([
    fakeToolInfo("exec_command", "npm:@howaboua/pi-codex-conversion"),
    fakeToolInfo("apply_patch", "npm:@howaboua/pi-codex-conversion"),
    fakeToolInfo("write_stdin", "npm:@howaboua/pi-codex-conversion"),
  ]);
  const warning = packageBypassWarning(pi);
  assert.ok(warning);
  for (const name of ["exec_command", "apply_patch", "write_stdin"]) {
    assert.match(warning, new RegExp(name));
  }
  // The package name itself should appear exactly once, not repeated per tool.
  const occurrences = warning.split("npm:@howaboua/pi-codex-conversion").length - 1;
  assert.equal(occurrences, 1, `expected the package name to be deduped, got:\n${warning}`);
});

test("packageBypassWarning names multiple distinct packages if more than one registers a bypass tool", () => {
  const pi = fakePiWithTools([
    fakeToolInfo("exec_command", "npm:@howaboua/pi-codex-conversion"),
    fakeToolInfo("web_run", "npm:@some-other-org/other-adapter"),
  ]);
  const warning = packageBypassWarning(pi);
  assert.ok(warning);
  assert.match(warning, /npm:@howaboua\/pi-codex-conversion/);
  assert.match(warning, /npm:@some-other-org\/other-adapter/);
});

test("packageBypassWarning does not flag an ordinary tool whose name happens to not be in BYPASS_TOOL_NAMES", () => {
  const pi = fakePiWithTools([fakeToolInfo("weave_edit", "npm:@howaboua/pi-codex-conversion")]);
  assert.equal(packageBypassWarning(pi), undefined);
});

test("packageBypassWarning degrades gracefully (does not throw) if sourceInfo is missing", () => {
  const pi = fakePiWithTools([fakeToolInfo("exec_command", undefined)]);
  assert.doesNotThrow(() => packageBypassWarning(pi));
  const warning = packageBypassWarning(pi);
  assert.ok(warning);
  assert.match(warning, /exec_command/);
});

// --- Full-lifecycle harness proving packageBypassWarning is actually wired
// into session_start (notified + stored) and shown in /pi-sem status, not
// just correct in isolation. Simulates another package already having
// registered a bypass tool by extension-load time (confirmed realistic:
// @howaboua/pi-codex-conversion's tools are already present in
// pi.getAllTools() by the time ANY session_start handler fires, since
// package registration happens at extension-load time, before session
// lifecycle events -- verified against a real `pi -e ... --mode json`
// session).

interface FakeRegisteredTool {
  name: string;
}

function makeHarness(cwd: string, externalTools: { name: string; sourceInfo: { source: string; origin: "package"; scope: "user"; path: string } }[]) {
  const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
  const registeredTools = new Map<string, FakeRegisteredTool>();
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
    setActiveTools: () => {},
    getActiveTools: () => [],
    getAllTools: () => [
      ...[...registeredTools.keys()].map((name) => ({ name, sourceInfo: { source: "pi-sem", origin: "top-level" as const, scope: "project" as const, path: "extensions/pi-sem.ts" } })),
      ...externalTools,
    ],
    appendEntry: () => {},
  } as unknown as ExtensionAPI;

  const ctx = {
    ui: { notify: (message: string, level?: string) => notices.push({ message, level }) },
    model: undefined,
    cwd,
  } as unknown as ExtensionContext;

  return { pi, ctx, handlers, notices, getCommandHandler: () => commandHandler };
}

async function bootSession(h: ReturnType<typeof makeHarness>) {
  piSemExtension(h.pi);
  for (const handler of h.handlers.get("session_start") ?? []) {
    await Promise.resolve(handler({ type: "session_start", reason: "startup" }, h.ctx)).catch(() => {});
  }
}

async function shutdownSession(h: ReturnType<typeof makeHarness>) {
  for (const handler of h.handlers.get("session_shutdown") ?? []) {
    await Promise.resolve(handler({ type: "session_shutdown", reason: "quit" }, h.ctx)).catch(() => {});
  }
}

function emptyServersConfigPath(dir: string): string {
  const configPath = join(dir, "config.json");
  writeFileSync(configPath, JSON.stringify({ servers: [], sessionPolicy: { activeBuiltins: ["bash", "write"] } } satisfies PiSemConfig), "utf8");
  return configPath;
}

test("session_start notifies the package-bypass warning when a bypass tool is already registered by another package", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-pkg-bypass-"));
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = emptyServersConfigPath(dir);

  const h = makeHarness(dir, [
    { name: "exec_command", sourceInfo: { source: "npm:@howaboua/pi-codex-conversion", origin: "package", scope: "user", path: "/fake" } },
  ]);

  try {
    await bootSession(h);
    const warningNotice = h.notices.find((n) => n.message.includes("exec_command"));
    assert.ok(warningNotice, `expected a notice mentioning exec_command, got: ${JSON.stringify(h.notices)}`);
    assert.match(warningNotice.message, /npm:@howaboua\/pi-codex-conversion/);
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("session_start does not notify any bypass warning when no bypass tool is registered", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-pkg-bypass-none-"));
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = emptyServersConfigPath(dir);

  const h = makeHarness(dir, []);

  try {
    await bootSession(h);
    const warningNotice = h.notices.find((n) => n.message.includes("bypass"));
    assert.equal(warningNotice, undefined, `expected no bypass warning, got: ${JSON.stringify(h.notices)}`);
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("/pi-sem status includes the package-bypass warning when one applies", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-pkg-bypass-status-"));
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = emptyServersConfigPath(dir);

  const h = makeHarness(dir, [
    { name: "apply_patch", sourceInfo: { source: "npm:@howaboua/pi-codex-conversion", origin: "package", scope: "user", path: "/fake" } },
  ]);

  try {
    await bootSession(h);
    const handler = h.getCommandHandler();
    assert.ok(handler, "pi-sem must register the /pi-sem command");
    await handler({}, h.ctx);

    const statusNotice = h.notices.find((n) => n.message.startsWith("mode:"));
    assert.ok(statusNotice);
    assert.match(statusNotice.message, /apply_patch/);
    assert.match(statusNotice.message, /npm:@howaboua\/pi-codex-conversion/);
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});
