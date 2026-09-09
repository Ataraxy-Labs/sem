import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import readline from "node:readline";

const here = dirname(fileURLToPath(import.meta.url));
const server = join(here, "../../src/transaction/server.mjs");

function repository() {
  const cwd = mkdtempSync(join(tmpdir(), "sem-transaction-"));
  execFileSync("git", ["init", "-q"], { cwd });
  execFileSync("git", ["config", "user.email", "test@example.com"], { cwd });
  execFileSync("git", ["config", "user.name", "Test"], { cwd });
  writeFileSync(join(cwd, "x.txt"), "one\n");
  execFileSync("git", ["add", "x.txt"], { cwd });
  execFileSync("git", ["commit", "-qm", "initial"], { cwd });
  return cwd;
}

function client(cwd: string) {
  const child = spawn(process.execPath, ["--experimental-strip-types", server], { cwd, stdio: ["pipe", "pipe", "pipe"] });
  const lines = readline.createInterface({ input: child.stdout });
  const pending = new Map<number, (value: any) => void>();
  lines.on("line", (line) => {
    const message = JSON.parse(line);
    pending.get(message.id)?.(message);
    pending.delete(message.id);
  });
  let id = 0;
  return {
    call(method: string, params: object = {}) {
      const requestId = ++id;
      const result = new Promise<any>((resolve) => pending.set(requestId, resolve));
      child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id: requestId, method, params })}\n`);
      return result;
    },
    close() { child.stdin.end(); },
  };
}

test("server exposes only the versioned plan and transaction protocol", async () => {
  const cwd = repository();
  const rpc = client(cwd);
  try {
    const listed = await rpc.call("tools/list");
    assert.deepEqual(listed.result.tools.map((tool: any) => tool.name), ["sem_plan", "weave_transaction"]);
    const planned = await rpc.call("tools/call", { name: "sem_plan", arguments: {} });
    const value = planned.result.structuredContent;
    assert.equal(value.protocol, "sem-transaction/1");
    assert.match(value.revision.digest, /^[0-9a-f]{64}$/);
    const duplicate = await rpc.call("tools/call", { name: "sem_plan", arguments: {} });
    assert.match(duplicate.error.message, /exactly one sem_plan/);
  } finally {
    rpc.close();
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("transaction refuses a workspace changed after its pinned plan", async () => {
  const cwd = repository();
  const rpc = client(cwd);
  try {
    await rpc.call("tools/call", { name: "sem_plan", arguments: {} });
    writeFileSync(join(cwd, "x.txt"), "changed elsewhere\n");
    const edited = await rpc.call("tools/call", { name: "weave_transaction", arguments: { edits: [] } });
    assert.match(edited.error.message, /workspace changed outside the transaction protocol/);
  } finally {
    rpc.close();
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("transaction rejects newly invented private collaborator access", async () => {
  const cwd = repository();
  const rpc = client(cwd);
  try {
    await rpc.call("tools/call", { name: "sem_plan", arguments: {} });
    const edited = await rpc.call("tools/call", {
      name: "weave_transaction",
      arguments: { edits: [{ file: "x.txt", old: "one", new: "context._secret" }] },
    });
    assert.match(edited.error.message, /invents private collaborator access/);
    assert.match(edited.error.message, /context\._secret/);
  } finally {
    rpc.close();
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("an explicit structural-kind change uses a revision-pinned exact replacement", async () => {
  const cwd = repository();
  writeFileSync(join(cwd, "sample.c"), "int answer(void) { return 42; }\n");
  execFileSync("git", ["add", "sample.c"], { cwd });
  execFileSync("git", ["commit", "-qm", "add sample"], { cwd });
  const rpc = client(cwd);
  try {
    await rpc.call("tools/call", { name: "sem_plan", arguments: { entity_names: ["answer"] } });
    const edited = await rpc.call("tools/call", {
      name: "weave_transaction",
      arguments: {
        edits: [{
          file: "sample.c",
          entity: { name: "answer", entity_type: "function" },
          op: "replace",
          content: "#define answer() 42",
          allow_signature_change: true,
        }],
      },
    });
    assert.equal(edited.result.structuredContent.exact_text_edits, 1);
    assert.equal(edited.result.structuredContent.edit.applied, 0);
    assert.match(readFileSync(join(cwd, "sample.c"), "utf8"), /#define answer\(\) 42/);
  } finally {
    rpc.close();
    rmSync(cwd, { recursive: true, force: true });
  }
});

test("a mixed transaction never reports committed when one structural edit rolls back", async () => {
  const cwd = repository();
  writeFileSync(join(cwd, "sample.c"), "int answer(void) { return 42; }\n");
  execFileSync("git", ["add", "sample.c"], { cwd });
  execFileSync("git", ["commit", "-qm", "add sample"], { cwd });
  const rpc = client(cwd);
  try {
    await rpc.call("tools/call", { name: "sem_plan", arguments: { entity_names: ["answer"] } });
    const edited = await rpc.call("tools/call", {
      name: "weave_transaction",
      arguments: {
        edits: [
          {
            file: "sample.c",
            entity: { name: "answer", entity_type: "function" },
            op: "replace",
            content: "#define answer() 42",
            allow_signature_change: true,
          },
          {
            file: "sample.c",
            entity: { name: "missing", entity_type: "function" },
            op: "replace",
            content: "int missing(void) { return 0; }",
          },
        ],
      },
    });
    assert.equal(edited.result.structuredContent.receipt.committed, false);
    assert.equal(edited.result.structuredContent.receipt.edits_applied, false);
  } finally {
    rpc.close();
    rmSync(cwd, { recursive: true, force: true });
  }
});
