import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { registerServerTools, type RegisterServerResult } from "../../src/bridge/register.ts";
import type { ServerConfig } from "../../src/config/types.ts";
import type { McpClient } from "../../src/bridge/mcp-client.ts";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FAKE_SERVER = join(__dirname, "fixtures", "fake-mcp-server.mjs");

/**
 * Vacuous-fixture audit follow-up (team-lead, after 83ea54f's code-mode
 * fail-open fix): before this file, `registerServerTools` -- the function
 * whose entire job is "filter an MCP server's tools/list result through
 * this server's allowlist, applying name/description/prompt overrides" --
 * had ZERO test coverage of that filtering/override logic anywhere in
 * test/bridge/**. The only existing direct caller,
 * register-tools-listtools-escape.review.test.ts, always passes `tools: {}`
 * and always hits the listTools()-CRASHES path deliberately (its own point
 * is "does a crash get captured, not thrown" -- the allowlist is correctly
 * irrelevant there, since the crash happens before any tool is ever
 * filtered). Every OTHER test/bridge/** file that constructs a ServerConfig
 * either uses `tools: {}` against a server that never starts/never answers
 * tools/list (extension-fail-closed.test.ts, the two failure-path tests in
 * extension-code-mode-fail-closed.test.ts) or never touches ServerConfig at
 * all. So the exact bug class that let 83ea54f's code-mode fail-open
 * survive an entire review campaign -- "every fake MCP server fixture used
 * an empty allowlist, so no test could observe what happens with a real
 * one" -- was ALSO true, independently, of registerServerTools's own core
 * contract. This file closes that gap directly: a real (fake) MCP server
 * process, actually started, actually answering tools/list with a real
 * tool ("echo", the same fixture mcp-client.test.ts uses), exercised
 * through registerServerTools itself with a variety of real (non-empty)
 * allowlist shapes.
 */

const fakePi: ExtensionAPI = (() => {
  const registered: Array<{ name: string; description?: string; promptSnippet?: string; promptGuidelines?: string[] }> = [];
  return {
    registerTool: (def: { name: string; description?: string; promptSnippet?: string; promptGuidelines?: string[] }) => {
      registered.push({ name: def.name, description: def.description, promptSnippet: def.promptSnippet, promptGuidelines: def.promptGuidelines });
    },
    __registered: registered,
  } as unknown as ExtensionAPI & { __registered: typeof registered };
})();

function makeConfig(tools: ServerConfig["tools"]): ServerConfig {
  return { id: "fake", command: process.execPath, args: [FAKE_SERVER], tools };
}

async function stop(outcome: { client: McpClient; result: RegisterServerResult }) {
  await outcome.client.stop();
}

test("registerServerTools registers an allowlisted tool (bare `true`) under its own name/description, and it's actually callable end-to-end", async () => {
  const pi = fakePi as ExtensionAPI & { __registered: Array<{ name: string; description?: string }> };
  pi.__registered.length = 0;
  const outcome = await registerServerTools(pi, makeConfig({ echo: true }));
  try {
    assert.equal(outcome.result.startError, undefined);
    assert.deepEqual(outcome.result.registeredToolNames, ["echo"], "the allowlisted tool must be registered under the MCP server's own name when override is bare `true`");
    assert.equal(pi.__registered.length, 1);
    assert.equal(pi.__registered[0]?.name, "echo");
    assert.equal(pi.__registered[0]?.description, "Echo the given text back", "bare `true` keeps the MCP tool's own description");
  } finally {
    await stop(outcome);
  }
});

test("a tool the healthy server actually offers, but that is NOT in the allowlist, is never registered", async () => {
  const pi = fakePi as ExtensionAPI & { __registered: Array<{ name: string }> };
  pi.__registered.length = 0;
  // Genuinely different scenario from every other `tools: {}` fixture in
  // this test suite: this server is healthy and DOES answer tools/list
  // with a real "echo" tool -- it's the allowlist itself, not a crash or
  // a dead server, that results in zero registrations.
  const outcome = await registerServerTools(pi, makeConfig({}));
  try {
    assert.equal(outcome.result.startError, undefined, "the server itself started and listed tools fine");
    assert.deepEqual(outcome.result.registeredToolNames, [], "a healthy server's tool not in the allowlist must not be registered");
    assert.equal(pi.__registered.length, 0);
  } finally {
    await stop(outcome);
  }
});

test("an override with a custom name registers under the renamed name and the overridden description, not the raw MCP ones", async () => {
  const pi = fakePi as ExtensionAPI & { __registered: Array<{ name: string; description?: string; promptSnippet?: string; promptGuidelines?: string[] }> };
  pi.__registered.length = 0;
  const outcome = await registerServerTools(
    pi,
    makeConfig({
      echo: { name: "custom_echo", description: "custom description", promptSnippet: "custom snippet", promptGuidelines: ["custom guideline"] },
    }),
  );
  try {
    assert.deepEqual(outcome.result.registeredToolNames, ["custom_echo"], "registeredToolNames must reflect the OVERRIDE name, not the raw MCP tool name");
    assert.equal(pi.__registered.length, 1);
    assert.equal(pi.__registered[0]?.name, "custom_echo");
    assert.equal(pi.__registered[0]?.description, "custom description");
    assert.equal(pi.__registered[0]?.promptSnippet, "custom snippet");
    assert.deepEqual(pi.__registered[0]?.promptGuidelines, ["custom guideline"]);
  } finally {
    await stop(outcome);
  }
});

test("a registered allowlisted tool's execute() actually calls the real MCP server and returns its result", async () => {
  const captured: Array<{ execute: (toolCallId: string, params: unknown, signal?: AbortSignal) => Promise<{ content: Array<{ type: string; text: string }> }> }> = [];
  const capturingPi = {
    registerTool: (def: unknown) => {
      captured.push(def as (typeof captured)[number]);
    },
  } as unknown as ExtensionAPI;

  const outcome = await registerServerTools(capturingPi, makeConfig({ echo: true }));
  try {
    assert.equal(captured.length, 1);
    const result = await captured[0]?.execute("call-1", { text: "hello from a real test" });
    assert.ok(result);
    const text = result.content.map((c) => c.text).join("\n");
    assert.match(text, /echo: hello from a real test/, "the registered tool's execute() must round-trip through the real server process, not a stub");
  } finally {
    await stop(outcome);
  }
});
