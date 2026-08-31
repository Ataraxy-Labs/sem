import { test } from "node:test";
import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { resolveCommand, resolveConfigCommands } from "../../src/config/resolve-command.ts";

/**
 * The vanished-binary incident: allowlist.json pointed the "weave" server at
 * an untracked cargo artifact that disappeared, and every consumer broke with
 * a confusing spawn ENOENT instead of a clear substitution. resolveCommand is
 * the single point that decides which binary actually runs; these tests pin
 * its order (env override > existing executable path > basename fallback) and
 * that bare names pass through untouched.
 *
 * (One test file, not two: the original change split unit tests and wiring
 * tests across resolve-command.test.ts + server-command-fallback.test.ts; the
 * wiring is one line in startServersAndRegisterTools and is covered here by
 * resolveConfigCommands over a realistic config. Removed vs original: the
 * live spawn-through-a-shimmed-$PATH test — it proved spawn()'s own $PATH
 * behavior, not this module's contract.)
 */

function collect(): { warnings: string[]; warn: (m: string) => void } {
  const warnings: string[] = [];
  return { warnings, warn: (m) => warnings.push(m) };
}

test("env override PI_SEM_<ID>_MCP_BIN wins unconditionally", () => {
  const { warnings, warn } = collect();
  process.env.PI_SEM_WEAVE_MCP_BIN = "/custom/weave-mcp";
  try {
    assert.equal(resolveCommand("weave", "/nonexistent/path/weave-mcp", warn), "/custom/weave-mcp");
    assert.equal(warnings.length, 0, "an explicit override is not a fallback; no warning");
  } finally {
    delete process.env.PI_SEM_WEAVE_MCP_BIN;
  }
});

test("an existing executable configured path passes through unchanged", () => {
  const dir = mkdtempSync(join(tmpdir(), "resolve-cmd-"));
  const bin = join(dir, "weave-mcp");
  writeFileSync(bin, "#!/bin/sh\nexit 0\n");
  chmodSync(bin, 0o755);
  const { warnings, warn } = collect();
  try {
    assert.equal(resolveCommand("weave", bin, warn), bin);
    assert.equal(warnings.length, 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a missing configured path falls back to its basename with one warning", () => {
  const { warnings, warn } = collect();
  assert.equal(resolveCommand("weave", "/gone/forever/weave-mcp", warn), "weave-mcp");
  assert.equal(warnings.length, 1);
  assert.match(warnings[0]!, /missing or not executable/);
  assert.match(warnings[0]!, /\/gone\/forever\/weave-mcp/);
});

test("a non-executable configured path falls back to its basename", () => {
  const dir = mkdtempSync(join(tmpdir(), "resolve-cmd-"));
  const bin = join(dir, "weave-mcp");
  writeFileSync(bin, "not a binary");
  chmodSync(bin, 0o644);
  const { warnings, warn } = collect();
  try {
    assert.equal(resolveCommand("weave", bin, warn), "weave-mcp");
    assert.equal(warnings.length, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("bare names (no slash) pass through untouched — spawn() owns $PATH resolution", () => {
  const { warnings, warn } = collect();
  assert.equal(resolveCommand("sem", "sem", warn), "sem");
  assert.equal(warnings.length, 0);
});

test("resolveConfigCommands maps every server without mutating the input", () => {
  const { warn } = collect();
  const config = {
    servers: [
      { id: "sem", command: "sem", tools: {} },
      { id: "weave", command: "/gone/weave-mcp", tools: {} },
    ],
    sessionPolicy: { activeBuiltins: ["bash", "write"] },
  };
  const resolved = resolveConfigCommands(config as never, warn);
  assert.equal(resolved.servers[0]!.command, "sem");
  assert.equal(resolved.servers[1]!.command, "weave-mcp");
  assert.equal(config.servers[1]!.command, "/gone/weave-mcp", "input config untouched");
});
