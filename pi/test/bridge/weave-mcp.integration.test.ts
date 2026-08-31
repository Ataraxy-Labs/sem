import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { McpClient } from "../../src/bridge/mcp-client.ts";

// Runs against the real weave-mcp binary the bridge core targets. Skips
// (rather than fails) when PI_SEM_WEAVE_MCP_BIN isn't set or doesn't point
// to a real binary, since that binary lives outside this package and is
// built separately (cargo build --release -p weave-mcp -p weave-cli
// -p weave-driver in the weave repo; standard target dir, which a release
// export typically excludes).
const WEAVE_MCP_ENV = process.env.PI_SEM_WEAVE_MCP_BIN;
const shouldRun = !!WEAVE_MCP_ENV && existsSync(WEAVE_MCP_ENV);
// Never actually used unless shouldRun is true (every test below is
// skipped otherwise) -- typed as a plain string so TypeScript doesn't
// need every call site to re-narrow `string | undefined`.
const WEAVE_MCP: string = WEAVE_MCP_ENV ?? "weave-mcp";

test(
  "weave-mcp: tools/list exposes >= 20 tools",
  { skip: !shouldRun && (WEAVE_MCP_ENV ? `weave-mcp binary not found at ${WEAVE_MCP_ENV}` : "PI_SEM_WEAVE_MCP_BIN is not set") },
  async () => {
    const client = new McpClient({ id: "weave", command: WEAVE_MCP, requestTimeoutMs: 15_000 });
    await client.start();
    try {
      const tools = await client.listTools();
      assert.ok(tools.length >= 20, `expected >= 20 tools, got ${tools.length}`);
      const names = tools.map((t) => t.name);
      assert.ok(names.includes("weave_extract_entities"), "weave_extract_entities should be listed");
    } finally {
      await client.stop();
    }
  },
);

test(
  "weave-mcp: weave_extract_entities on a real fixture file",
  { skip: !shouldRun && (WEAVE_MCP_ENV ? `weave-mcp binary not found at ${WEAVE_MCP_ENV}` : "PI_SEM_WEAVE_MCP_BIN is not set") },
  async () => {
    const dir = mkdtempSync(join(tmpdir(), "pi-sem-weave-fixture-"));
    const fixture = join(dir, "fixture.ts");
    writeFileSync(
      fixture,
      [
        "export function add(a: number, b: number): number {",
        "  return a + b;",
        "}",
        "",
        "export class Calculator {",
        "  total = 0;",
        "  addToTotal(n: number) {",
        "    this.total = add(this.total, n);",
        "    return this.total;",
        "  }",
        "}",
        "",
      ].join("\n"),
    );

    // weave-mcp resolves relative file_path args against a git repo root.
    execFileSync("git", ["init", "-q"], { cwd: dir });
    execFileSync("git", ["add", "fixture.ts"], { cwd: dir });
    execFileSync("git", ["-c", "user.email=test@test.com", "-c", "user.name=test", "commit", "-q", "-m", "init"], { cwd: dir });

    const client = new McpClient({ id: "weave", command: WEAVE_MCP, cwd: dir, requestTimeoutMs: 15_000 });
    await client.start();
    try {
      const result = await client.callTool("weave_extract_entities", { file_path: "fixture.ts" });
      assert.ok(!result.isError, `tool call should not error: ${result.content[0]?.text}`);
      const text = result.content.map((c) => c.text).join("\n");
      assert.match(text, /add/, "extracted entities should mention the add function");
      assert.match(text, /Calculator/, "extracted entities should mention the Calculator class");
    } finally {
      await client.stop();
      rmSync(dir, { recursive: true, force: true });
    }
  },
);
