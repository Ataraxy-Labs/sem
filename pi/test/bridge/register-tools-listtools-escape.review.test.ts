import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { registerServerTools, type RegisterServerResult } from "../../src/bridge/register.ts";
import type { ServerConfig } from "../../src/config/types.ts";
import type { McpClient } from "../../src/bridge/mcp-client.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_SERVER = join(__dirname, "fixtures", "fake-mcp-server-crash-on-tools-list.mjs");

// Minimal ExtensionAPI stub: registerServerTools only ever calls
// pi.registerTool(...) once it has a tool list, which this scenario never
// reaches.
const fakePi = { registerTool: () => {} } as unknown as Parameters<typeof registerServerTools>[0];

/**
 * A category b finding: tool
 * registration / "does a client.listTools() failure get captured into
 * RegisterServerResult.startError, or does it escape uncaught" —
 * finding #7, graded CONFIRMED, plus a second defect this test's own
 * failure mode exposed (below).
 *
 * ============================================================
 * Defect 1: registerServerTools breaks its own documented contract
 * ============================================================
 * src/bridge/register.ts (lines 65-78):
 *
 *   try {
 *     await client.start();
 *   } catch (err) {
 *     return { client, result: { serverId, registeredToolNames: [], startError: err } };
 *   }
 *
 *   const tools = await client.listTools();   // <-- NOT try/caught
 *
 * `RegisterServerResult.startError` is documented as "Set when the server
 * failed to start; the server contributes zero tools" — i.e. the whole
 * function's contract is "server-start-adjacent failures are captured as a
 * result value, never thrown." Only `client.start()` is guarded. If the
 * child completes `initialize` (so `start()` resolves) and then
 * dies/errors before answering `tools/list`, the rejection propagates
 * straight out of `registerServerTools`, breaking that contract. The
 * caller (extensions/pi-sem.ts's `startServersAndRegisterTools`, invoked
 * from a `session_start` handler that pi's extension runner wraps in
 * try/catch-and-swallow-into-emitError — verified in
 * node_modules/@earendil-works/pi-coding-agent/dist/core/extensions/runner.js
 * around `emit()`) never reaches `pi.setActiveTools(...)`, so the intended
 * tool lockdown silently never happens for that session.
 *
 * ============================================================
 * Defect 2 (discovered empirically while writing this test — it made the
 * naive version of this test hang forever): the client instance created
 * inside registerServerTools is unreachable once the function rejects, so
 * NOTHING can ever call .stop() on it. Worse, McpClient.onExit()
 * (src/bridge/mcp-client.ts:127-145) respawns unconditionally on any
 * unexpected exit ("one allowed restart"), with no way to know its owner
 * has already abandoned it after the listTools() rejection. That respawn
 * fires a SECOND, fully independent child process that answers its own
 * `initialize` normally and then just sits there listening on stdin
 * forever — nobody holds a reference to it, nobody will ever call stop()
 * on it, and its open stdio pipes keep the process's event loop alive.
 * ============================================================
 *
 * Untested prior to this file: no existing test in test/bridge/*.test.ts
 * exercises a listTools() failure; the closest tests (mcp-client.test.ts's
 * crash tests) exercise the lower-level McpClient's restart behavior
 * directly with a client the test itself owns and can stop(), not
 * registerServerTools's documented result-capturing contract or its
 * client-ownership story.
 *
 * Fix sketch (defect 1): widen the try/catch in registerServerTools to
 * cover both `client.start()` and `client.listTools()`, returning the
 * client either way so the caller can always stop() it.
 * Fix sketch (defect 2): registerServerTools should stop() the client
 * itself before rethrowing/returning on any listTools() failure, and/or
 * onExit()'s respawn should be caller-cancelable (e.g. gated on a `dead`-like
 * flag the owner can set before abandoning the client).
 */
test("registerServerTools captures a listTools() failure into startError instead of letting it escape uncaught, and does not orphan a live respawned child process", async () => {
  const dir = mkdtempSync(join(tmpdir(), "pi-sem-register-crash-"));
  const spawnLog = join(dir, "spawns.log");
  const strayPids: number[] = [];

  try {
    const config: ServerConfig = {
      id: "crash-on-tools-list",
      command: process.execPath,
      args: [FAKE_SERVER],
      env: { FAKE_MCP_SPAWN_LOG: spawnLog },
      tools: {},
    };

    let outcome: { client: McpClient; result: RegisterServerResult } | undefined;
    let thrown: unknown;
    try {
      outcome = await registerServerTools(fakePi, config);
    } catch (err) {
      thrown = err;
    } finally {
      await outcome?.client.stop();
    }

    assert.equal(
      thrown,
      undefined,
      `registerServerTools should capture a listTools() failure as result.startError per its documented contract, not throw. It threw: ${thrown instanceof Error ? thrown.stack : String(thrown)}`,
    );
    assert.ok(outcome, "registerServerTools should have returned a result");
    assert.ok(
      outcome.result.startError,
      "result.startError should be set when listTools() fails, mirroring the client.start() failure path",
    );
    assert.deepEqual(outcome.result.registeredToolNames, [], "no tools should be registered when listTools() fails");

    // Defect 2, made concrete: because the current code throws instead of
    // returning, `outcome` above is never assigned — there is no client
    // reference for anything to stop(). onExit()'s "one allowed restart"
    // still fires though, producing a second fixture process nobody owns.
    // Wait briefly for the respawn to happen, then prove it did.
    await new Promise((resolve) => setTimeout(resolve, 300));
    const spawnedPids = existsSync(spawnLog)
      ? readFileSync(spawnLog, "utf8").trim().split("\n").filter(Boolean).map(Number)
      : [];
    assert.ok(
      spawnedPids.length >= 2,
      `expected an unowned respawn to have produced a second fixture process (zombie); observed PIDs: ${JSON.stringify(spawnedPids)}`,
    );
    strayPids.push(...spawnedPids);
  } finally {
    // Test-hygiene cleanup so this defect doesn't hang the suite: reap any
    // fixture processes still alive, however many respawns happened.
    for (const pid of strayPids) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // Already dead — fine.
      }
    }
    rmSync(dir, { recursive: true, force: true });
  }
});
