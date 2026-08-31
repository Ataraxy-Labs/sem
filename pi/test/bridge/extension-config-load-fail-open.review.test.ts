import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import piSemExtension from "../../extensions/pi-sem.ts";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * A supplementary category-a finding, graded CONFIRMED — a real gap
 * missed on first pass, caught independently on a follow-up read.
 *
 * extensions/pi-sem.ts's session_start handler (default export):
 *
 *   pi.on("session_start", async (_event, ctx) => {
 *     if (started) return;
 *     started = true;
 *
 *     config = await loadConfig();                          // <-- line ~135, unguarded
 *     const outcome = await startServersAndRegisterTools(pi, ctx, config);   // <-- the fail-closed machinery
 *     ...
 *     pi.registerTool({ name: "bash", ... });                // the audit wrapper
 *     ...
 *   });
 *
 * `startServersAndRegisterTools` was hardened (this same review round) to
 * be fail-closed no matter how badly ITS OWN servers fail —
 * `computeActiveTools` + `pi.setActiveTools(...)` run unconditionally as
 * its last statement. But `loadConfig()` runs BEFORE
 * `startServersAndRegisterTools` is ever called at all. `src/config/load.ts`
 * has multiple unguarded throw points when `PI_SEM_CONFIG` is set:
 * `readFile(...)` (ENOENT), `JSON.parse(raw)` (malformed JSON), or a
 * throwing/missing-export dynamic `import()` for a .ts/.js override. Any of
 * these rejects the whole `session_start` handler BEFORE
 * `startServersAndRegisterTools` (the fail-closed code) or the bash-audit
 * wrapper registration ever run. Per pi's extension runner (`emit()` in
 * `@earendil-works/pi-coding-agent`, verified in the first review round),
 * a rejected `session_start` handler is caught and swallowed into
 * `emitError` — the session simply proceeds with pi's completely
 * untouched default toolset (read/edit/bash/write, no audit wrapper
 * either) and no `weave_edit`/`sem_*` tools registered at all. This is
 * exactly the failure mode "fail-closed" was built to prevent, reopened by
 * an ordering gap the fix didn't cover.
 *
 * Untested: test/bridge/extension-fail-closed.test.ts calls
 * `startServersAndRegisterTools` directly, which starts AFTER `loadConfig()`
 * already succeeded in every one of its tests — none of them go through the
 * actual `piSemExtension` default export or set a broken `PI_SEM_CONFIG`.
 *
 * Fix sketch: wrap `config = await loadConfig()` in its own try/catch
 * inside the session_start handler, falling back to `DEFAULT_CONFIG` (or
 * at least still calling `startServersAndRegisterTools` with a safe
 * config / still registering the bash wrapper and setting a fail-closed
 * active-tools set) on failure, with a `ctx.ui.notify` warning.
 */
test("a PI_SEM_CONFIG that fails to load still leaves the session fail-closed, not on pi's full default toolset", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-config-fail-open-"));
  const brokenConfigPath = join(dir, "broken.json");
  writeFileSync(brokenConfigPath, "{ this is not valid json", "utf8");

  const previousEnv = process.env.PI_SEM_CONFIG;
  process.env.PI_SEM_CONFIG = brokenConfigPath;

  // Real pi supports multiple independent extensions/call-sites registering
  // handlers for the same event name -- e.g. piSemExtension's own
  // session_shutdown (stops the MCP clients it spawned) and
  // registerWeaveEdit's session_shutdown (stops its own coordinator) both
  // fire. A single-handler-per-event Map (overwrite semantics) silently
  // drops the earlier registration, which here meant piSemExtension's own
  // client-cleanup handler never ran and the real spawned sem/weave-mcp
  // child processes leaked past the test, hanging the runner. Store lists
  // and invoke all of them, matching real pi's fan-out behavior.
  const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => unknown>>();
  const activeToolsCalls: string[][] = [];
  const registeredToolNames: string[] = [];
  const notices: { message: string; level?: string }[] = [];

  const pi = {
    on: (event: string, handler: (event: unknown, ctx: unknown) => unknown) => {
      const existing = handlers.get(event) ?? [];
      existing.push(handler);
      handlers.set(event, existing);
    },
    registerTool: (def: { name: string }) => {
      registeredToolNames.push(def.name);
    },
    registerCommand: () => {},
    setActiveTools: (names: string[]) => {
      activeToolsCalls.push(names);
    },
    getActiveTools: () => activeToolsCalls.at(-1) ?? [],
    getAllTools: () => registeredToolNames.map((name) => ({ name })),
    appendEntry: () => {},
  } as unknown as ExtensionAPI;

  const ctx = {
    ui: { notify: (message: string, level?: string) => notices.push({ message, level }) },
    model: undefined,
    cwd: process.cwd(),
  } as unknown as ExtensionContext;

  try {
    piSemExtension(pi);
    const sessionStartHandlers = handlers.get("session_start") ?? [];
    assert.ok(sessionStartHandlers.length > 0, "extension must register a session_start handler");

    // Mirrors how pi's real ExtensionRunner.emit() behaves (verified against
    // @earendil-works/pi-coding-agent in the first review round): a handler
    // rejection is caught by the runner itself, not by the extension. We
    // reproduce that boundary here so the test observes exactly what a real
    // session would see -- state left however the handler left it before
    // throwing, nothing more.
    for (const sessionStart of sessionStartHandlers) {
      await Promise.resolve(sessionStart({ type: "session_start", reason: "startup" }, ctx)).catch(() => {});
    }

    assert.ok(
      activeToolsCalls.length >= 1,
      `pi.setActiveTools should still have been called even though PI_SEM_CONFIG failed to load -- it was never called at all, ` +
        `so the session is left on pi's full, unrestricted default toolset. Notices seen: ${JSON.stringify(notices)}`,
    );

    // FOLLOW-UP (after the vacuous-fixture audit / b21eae7): this
    // assertion used to stop at "setActiveTools was called at all" -- it
    // never checked what was actually IN that call. The fallback at the
    // time was DEFAULT_CONFIG, which spawns the real sem/weave-mcp
    // binaries and grants the FULL curated bridged-tool surface
    // (sem_impact/sem_diff/weave_impact/etc) -- a broader authority grant
    // than an operator who explicitly set a (now-unreadable) PI_SEM_CONFIG
    // ever actually made. A config we can't parse is a config whose
    // intended allowlist we don't know; granting the default allowlist in
    // that state is the same "authority we didn't intend" pattern as the
    // code-mode fail-open (83ea54f) -- so this now fails CLOSED: no
    // bridged MCP-server tools, native tools + bash/write only. Pinning
    // the EXACT set, not just "no bridged names", so a future regression
    // (e.g. some native tool silently dropped) is caught too.
    assert.deepEqual(
      activeToolsCalls[0],
      [
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
      ],
      "an unreadable/malformed PI_SEM_CONFIG must fail CLOSED: native tools + bash/write only, never DEFAULT_CONFIG's broader bridged-server allowlist",
    );
    for (const bridgedShaped of ["sem_impact", "sem_diff", "sem_blame", "sem_log", "sem_context", "sem_entities", "weave_impact", "weave_status", "weave_diff"]) {
      assert.ok(!registeredToolNames.includes(bridgedShaped), `${bridgedShaped} must not be registered when PI_SEM_CONFIG fails to load`);
    }

    // The fix falls back to DEFAULT_CONFIG, which spawns real sem/weave-mcp
    // child processes -- exactly what a real pi session_shutdown would stop.
    // Run EVERY registered session_shutdown handler (piSemExtension's own
    // client-cleanup handler AND registerWeaveEdit's own coordinator
    // cleanup, both registered under the same event name) so this test
    // doesn't leak them and hang the runner.
    const sessionShutdownHandlers = handlers.get("session_shutdown") ?? [];
    for (const sessionShutdown of sessionShutdownHandlers) {
      await Promise.resolve(sessionShutdown({ type: "session_shutdown", reason: "quit" }, ctx)).catch(() => {});
    }
  } finally {
    if (previousEnv === undefined) delete process.env.PI_SEM_CONFIG;
    else process.env.PI_SEM_CONFIG = previousEnv;
    rmSync(dir, { recursive: true, force: true });
  }
});
