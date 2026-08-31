import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import piSemExtension from "../../extensions/pi-sem.ts";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * Pass-2 review item 2: `write` must be audited the same way `bash` is --
 * override it by name, delegate real execution, log a pi-sem-write-audit
 * entry (path, bytes, whether the target looks like a code file), and
 * under PI_SEM_STRICT=1 refuse writes to EXISTING code files (new files
 * always allowed) with "use weave_edit for existing entities; write is for
 * new files". Counted in /pi-sem status.
 *
 * See test/bridge/write-audit.test.ts for the pure classifier's own unit
 * tests; this file proves the extension actually wires it up: real
 * delegation to pi's built-in write tool, real audit-log entries via
 * pi.appendEntry, and the strict-mode refusal path end to end.
 */

interface FakeWriteToolDef {
  name: string;
  // Widened to accept both write's {path, content} and bash's {command} --
  // registeredTools is one shared map for every tool this harness captures.
  execute: (
    toolCallId: string,
    params: { path: string; content: string } | { command: string },
    signal: AbortSignal | undefined,
    onUpdate: unknown,
    execCtx: unknown,
  ) => Promise<unknown>;
}

function makeHarness(cwd: string) {
  // Real pi fans out to every handler registered for an event name (see
  // extension-config-load-fail-open.review.test.ts for why a single-handler
  // Map is wrong here: registerWeaveEdit also registers its own
  // session_shutdown, and overwrite semantics would drop piSemExtension's
  // own client-cleanup handler, leaking the real sem/weave-mcp child
  // processes DEFAULT_CONFIG spawns).
  const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
  const registeredTools = new Map<string, FakeWriteToolDef>();
  const appendedEntries: { kind: string; data: unknown }[] = [];
  const activeToolsCalls: string[][] = [];
  const notices: { message: string; level?: string }[] = [];

  const pi = {
    on: (event: string, handler: (event: unknown, ctx: unknown) => unknown) => {
      const existing = handlers.get(event) ?? [];
      existing.push(handler);
      handlers.set(event, existing);
    },
    registerTool: (def: FakeWriteToolDef) => {
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
    appendEntry: (kind: string, data: unknown) => {
      appendedEntries.push({ kind, data });
    },
  } as unknown as ExtensionAPI;

  let commandHandler: ((args: unknown, ctx: unknown) => Promise<void>) | undefined;

  const ctx = {
    ui: { notify: (message: string, level?: string) => notices.push({ message, level }) },
    model: undefined,
    cwd,
  } as unknown as ExtensionContext;

  return {
    pi,
    ctx,
    handlers,
    registeredTools,
    appendedEntries,
    notices,
    getCommandHandler: () => commandHandler,
  };
}

async function bootSession(h: ReturnType<typeof makeHarness>) {
  piSemExtension(h.pi);
  const sessionStartHandlers = h.handlers.get("session_start") ?? [];
  for (const handler of sessionStartHandlers) {
    await Promise.resolve(handler({ type: "session_start", reason: "startup" }, h.ctx)).catch(() => {});
  }
}

async function shutdownSession(h: ReturnType<typeof makeHarness>) {
  const sessionShutdownHandlers = h.handlers.get("session_shutdown") ?? [];
  for (const handler of sessionShutdownHandlers) {
    await Promise.resolve(handler({ type: "session_shutdown", reason: "quit" }, h.ctx)).catch(() => {});
  }
}

test("write is registered by name, delegates real execution, and logs a pi-sem-write-audit entry for a new file", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-write-audit-new-"));
  const h = makeHarness(dir);

  try {
    await bootSession(h);

    const writeTool = h.registeredTools.get("write");
    assert.ok(writeTool, 'pi-sem must register a tool named "write"');

    const targetPath = join(dir, "brand-new.ts");
    const content = "export const x = 1;\n";
    const result = await writeTool.execute("call-1", { path: targetPath, content }, undefined, () => {}, {});
    assert.ok(result, "write execute should delegate to the real write implementation and return its result");
    assert.equal(readFileSync(targetPath, "utf8"), content, "the real write must actually have happened on disk");

    const writeAuditEntries = h.appendedEntries.filter((e) => e.kind === "pi-sem-write-audit");
    assert.equal(writeAuditEntries.length, 1, "a pi-sem-write-audit entry must be logged for every write");
    const entry = writeAuditEntries[0]?.data as { path: string; bytes: number; isCodeFile: boolean; targetExists: boolean; refused: boolean };
    assert.equal(entry.isCodeFile, true);
    assert.equal(entry.targetExists, false);
    assert.equal(entry.refused, false);
    assert.equal(entry.bytes, Buffer.byteLength(content, "utf8"));
  } finally {
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("PI_SEM_STRICT=1 refuses a write that would overwrite an existing code file, without touching the file on disk", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-write-audit-strict-refuse-"));
  const existingPath = join(dir, "existing.ts");
  const originalContent = "export const old = 1;\n";
  writeFileSync(existingPath, originalContent, "utf8");
  const h = makeHarness(dir);

  const previousStrict = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_STRICT = "1";

  try {
    await bootSession(h);
    const writeTool = h.registeredTools.get("write");
    assert.ok(writeTool);

    await assert.rejects(
      () => writeTool.execute("call-2", { path: existingPath, content: "export const old = 2;\n" }, undefined, () => {}, {}),
      /use weave_edit for existing entities; write is for new files/,
      "refusing a write to an existing code file under strict mode must carry the required guidance",
    );

    assert.equal(readFileSync(existingPath, "utf8"), originalContent, "a refused write must never touch the file on disk");

    const writeAuditEntries = h.appendedEntries.filter((e) => e.kind === "pi-sem-write-audit");
    assert.equal(writeAuditEntries.length, 1);
    const entry = writeAuditEntries[0]?.data as { refused: boolean; targetExists: boolean; isCodeFile: boolean };
    assert.equal(entry.refused, true);
    assert.equal(entry.targetExists, true);
    assert.equal(entry.isCodeFile, true);
  } finally {
    if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = previousStrict;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("PI_SEM_STRICT=1 still allows a write to a brand-new file (never refuses new files)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-write-audit-strict-new-"));
  const h = makeHarness(dir);

  const previousStrict = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_STRICT = "1";

  try {
    await bootSession(h);
    const writeTool = h.registeredTools.get("write");
    assert.ok(writeTool);

    const targetPath = join(dir, "totally-new.ts");
    const content = "export const brandNew = true;\n";
    await writeTool.execute("call-3", { path: targetPath, content }, undefined, () => {}, {});
    assert.equal(readFileSync(targetPath, "utf8"), content, "a write to a new file must go through even under strict mode");
  } finally {
    if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = previousStrict;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("PI_SEM_STRICT=1 still allows overwriting an existing NON-code file", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-write-audit-strict-noncode-"));
  const existingPath = join(dir, "notes.md");
  writeFileSync(existingPath, "old notes\n", "utf8");
  const h = makeHarness(dir);

  const previousStrict = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_STRICT = "1";

  try {
    await bootSession(h);
    const writeTool = h.registeredTools.get("write");
    assert.ok(writeTool);

    await writeTool.execute("call-4", { path: existingPath, content: "new notes\n" }, undefined, () => {}, {});
    assert.equal(readFileSync(existingPath, "utf8"), "new notes\n", "overwriting a non-code file must go through even under strict mode");
  } finally {
    if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = previousStrict;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("/pi-sem status counts write audit events for the session", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-write-audit-status-"));
  // This test's claim is about the status command's write-audit COUNTER, which
  // has nothing to do with whether a real external MCP server binary exists on
  // this machine. Booting through DEFAULT_CONFIG spawns the real weave-mcp
  // binary (an untracked, machine-local cargo artifact); when it is absent the
  // per-server failure notice lands in h.notices and the strict
  // `notices.length === 1` assertion below counts it. Scope the boot to zero
  // servers so the test measures only what it claims to measure.
  const configPath = join(dir, "config.json");
  writeFileSync(configPath, JSON.stringify({ servers: [], sessionPolicy: { activeBuiltins: ["bash", "write"] } }), "utf8");
  const previousConfig = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = configPath;
  const h = makeHarness(dir);

  try {
    await bootSession(h);
    const writeTool = h.registeredTools.get("write");
    assert.ok(writeTool);

    await writeTool.execute("call-5", { path: join(dir, "a.ts"), content: "export const a = 1;\n" }, undefined, () => {}, {});
    await writeTool.execute("call-6", { path: join(dir, "b.md"), content: "# notes\n" }, undefined, () => {}, {});

    const handler = h.getCommandHandler();
    assert.ok(handler, "pi-sem must register the /pi-sem command");
    await handler({}, h.ctx);

    assert.equal(h.notices.length, 1);
    const statusText = h.notices[0]?.message ?? "";
    assert.match(statusText, /write audit events this session: 2/, `expected write audit count in /pi-sem status, got:\n${statusText}`);
  } finally {
    if (previousConfig === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousConfig;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

// --- ox-review-3 finding #19: bash-side writes bypassed PI_SEM_STRICT's
// write protection entirely. src/bridge/bash-audit.ts's own unit tests
// (bash-audit.test.ts) cover the pure classifier + resolver wiring with a
// FAKE resolveTargetExists; this proves the real wiring end to end -- the
// bash tool wrapper in extensions/pi-sem.ts passes a resolver backed by a
// REAL existsSync(resolvePath(ctx.cwd, path)) call against a real file on
// disk, not a mock.

test("PI_SEM_STRICT=1 refuses a bash sed -i on an EXISTING code file on real disk, same message as the write wrapper", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-bash-write-audit-"));
  const existingPath = join(dir, "existing.ts");
  const originalContent = "export const old = 1;\n";
  writeFileSync(existingPath, originalContent, "utf8");
  const h = makeHarness(dir);

  const previousStrict = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_STRICT = "1";

  try {
    await bootSession(h);
    const bashTool = h.registeredTools.get("bash");
    assert.ok(bashTool);

    await assert.rejects(
      () => bashTool.execute("call-1", { command: `sed -i 's/old/new/' ${existingPath}` }, undefined, () => {}, { cwd: dir }),
      /use weave_edit for existing entities; write is for new files/,
      "a bash sed -i on an existing code file under strict mode must be refused with the SAME message the builtin write wrapper uses",
    );

    assert.equal(readFileSync(existingPath, "utf8"), originalContent, "a refused bash write must never actually touch the file on disk");
  } finally {
    if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = previousStrict;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("PI_SEM_STRICT=1 still allows a bash sed -i on a NEW file on real disk (never refuses a file that doesn't exist yet)", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-bash-write-audit-new-"));
  const newPath = join(dir, "brand-new.ts");
  writeFileSync(newPath, "export const x = 1;\n", "utf8");
  // Delete it again immediately -- we want a path that genuinely doesn't
  // exist on disk when the bash tool runs, proving the REAL existsSync
  // check (not a hardcoded true) drives the decision.
  rmSync(newPath);
  const h = makeHarness(dir);

  const previousStrict = process.env.PI_SEM_STRICT;
  process.env.PI_SEM_STRICT = "1";

  try {
    await bootSession(h);
    const bashTool = h.registeredTools.get("bash");
    assert.ok(bashTool);

    // pi's real bash implementation needs a fuller execCtx (session
    // machinery this test harness doesn't build) to actually run a
    // command -- so this WILL still throw past the audit layer. What we're
    // actually proving is that it's NOT pi-sem's own policy doing the
    // throwing: the resolver correctly reports "doesn't exist" for this
    // path, so strict mode never refuses it, and appendEntry's own logged
    // decision confirms `refused: false` regardless of what happens deeper
    // in the delegated real execution.
    await bashTool.execute("call-1", { command: `echo hi > ${newPath}` }, undefined, () => {}, { cwd: dir }).catch((err: unknown) => {
      const message = err instanceof Error ? err.message : String(err);
      assert.ok(!message.includes("use weave_edit"), `must not be pi-sem's own write refusal, got: ${message}`);
    });

    const auditEntry = h.appendedEntries.find((e) => e.kind === "pi-sem-bash-audit");
    assert.ok(auditEntry, "a pi-sem-bash-audit entry must be logged for this write");
    const entryData = auditEntry.data as { refused: boolean };
    assert.equal(entryData.refused, false, "a write to a nonexistent path must never be refused, even under strict mode");
  } finally {
    if (previousStrict === undefined) delete process.env.PI_SEM_STRICT;
    else process.env.PI_SEM_STRICT = previousStrict;
    await shutdownSession(h);
    rmSync(dir, { recursive: true, force: true });
  }
});
