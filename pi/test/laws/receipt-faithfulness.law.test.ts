// LAW 2 -- RECEIPT FAITHFULNESS.
//
// STRUCTURE: a successful edit's return value is a RECEIPT -- the
// caller-visible denotation of a disk transition. The law is that the
// receipt is a faithful abstraction of that transition (compare images,
// never representations -- Hoare 1972, "Proof of Correctness of Data
// Representations"): every claim the receipt makes must be re-derivable
// from the bytes on disk after the call:
//
//   (a) landed        => the edit's text is on disk, at the lines the
//                        receipt's new_range names;
//   (b) merge.performed => what landed is the merge engine's OUTPUT
//                        (ours + theirs), never ours_content alone, and
//                        mergedOver names only what changed UNDERNEATH
//                        (this edit's own entity is filtered out);
//   (c) impact        => the line names only entities that actually
//                        reference the edited one -- bystanders in the
//                        same repo never appear.
//
// (b)'s deterministic half runs against the fake backstop server; its LIVE
// half (the real weave-mcp merge engine, real drift injected mid-flight
// via the laws-gated-sem seam) uses the live lane's skip-if-absent
// convention, same as test/tools/weave-merge-backstop-live.test.ts.
//
// Non-vacuity probes: the (c) test first proves the bystander entity is
// real and findable (so its absence from the impact line is a filtering
// fact, not a fixture accident), and the live (b) test asserts drift was
// actually detected (merge.performed true) before trusting anything else.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync, spawnSync } from "node:child_process";
import { performWeaveEdit, type MergeStatus, type WeaveEditOutcome } from "../../src/tools/weave-edit.ts";
import { Coordinator } from "../../src/tools/internal/weave-coordination.ts";
import { buildSemApi } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const BACKSTOP_FAKE = join(__dirname, "..", "tools", "fixtures", "fake-weave-merge-backstop-server.mjs");
const LAWS_GATED_SEM = join(__dirname, "fixtures", "laws-gated-sem.mjs");

function resolveWeaveMcp(): string | undefined {
  const fromEnv = process.env.PI_SEM_WEAVE_MCP_BIN;
  if (fromEnv && spawnSync(fromEnv, ["--version"], { stdio: "ignore" }).error === undefined) return fromEnv;
  if (spawnSync("weave-mcp", ["--version"], { stdio: "ignore" }).error === undefined) return "weave-mcp";
  return undefined;
}
const WEAVE_MCP = resolveWeaveMcp();
const LIVE_SKIP = WEAVE_MCP === undefined ? { skip: "no weave-mcp binary (set PI_SEM_WEAVE_MCP_BIN or put weave-mcp on PATH)" } : {};

const ALPHA = "export function alpha(x: number): number {\n  return x + 1;\n}";
const ALPHA_OURS = "export function alpha(x: number): number {\n  return x + 42;\n}";
const BETA = "export function beta(y: number): number {\n  return y * 2;\n}";
const BETA_THEIRS = "export function beta(y: number): number {\n  return y * 9;\n}";
const FILE_V0 = `${ALPHA}\n\n${BETA}\n`;

function makeRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "law-receipt-"));
  writeFileSync(join(dir, "calc.ts"), FILE_V0);
  execFileSync("git", ["init", "-q", "-b", "main"], { cwd: dir });
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  return dir;
}

test("(a) landed: the receipt's new_range names the exact lines where the edit's bytes actually sit on disk", async () => {
  const dir = mkdtempSync(join(tmpdir(), "law-receipt-"));
  try {
    writeFileSync(join(dir, "calc.ts"), FILE_V0);
    const outcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "beta" }, op: "replace", content: BETA_THEIRS },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, false, outcome.text);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.ok(disk.includes(BETA_THEIRS), "the receipt says landed, so the bytes must be on disk");
    const newRange = (outcome.details as { new_range: { start_line: number; end_line: number } }).new_range;
    const lines = disk.split("\n");
    const claimed = lines.slice(newRange.start_line - 1, newRange.end_line).join("\n");
    assert.equal(claimed, BETA_THEIRS, "new_range must address exactly the edited entity's lines, re-derived from disk");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("(b) merge.performed: what lands is the merge OUTPUT (ours + theirs), and mergedOver excludes this edit's own entity", async () => {
  const dir = makeRepo();
  const coordinator = new Coordinator({
    command: process.execPath,
    args: [BACKSTOP_FAKE],
    env: { FAKE_MERGE_MODE: "clean", FAKE_MERGE_SUB: `${BETA}|||${BETA_THEIRS}`, FAKE_MERGE_OVER: "beta,alpha" },
    cwd: dir,
    agentId: "law-receipt-merge-agent",
  });
  try {
    const outcome = await performWeaveEdit(
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_OURS },
      { cwd: dir, semBin: "sem", coordinator },
    );
    assert.equal(outcome.isError, false, outcome.text);
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.ok(disk.includes("x + 42"), "ours survived the merge onto disk");
    assert.ok(disk.includes("y * 9"), "theirs was carried onto disk -- ours_content alone did NOT land");
    const merge = outcome.details.merge as MergeStatus;
    assert.equal(merge.performed, true);
    assert.deepEqual(merge.mergedOver, ["beta"], "mergedOver names what changed UNDERNEATH -- this edit's own entity (alpha) is filtered out even when the server lists it");
    assert.match(outcome.text, /Merged over concurrent changes to beta/);
  } finally {
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("(c) impact: the line names only entities that reference the edited one -- a real, findable bystander never appears", async () => {
  const dir = mkdtempSync(join(tmpdir(), "law-receipt-impact-"));
  try {
    writeFileSync(join(dir, "core.ts"), "export function target(): number {\n  return 1;\n}\n");
    writeFileSync(join(dir, "user.ts"), 'import { target } from "./core.ts";\n\nexport function caller1(): number {\n  return target();\n}\n');
    writeFileSync(join(dir, "bystander.ts"), "export function unrelated(): number {\n  return 7;\n}\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    // Non-vacuity: the bystander is a real, extractable entity in the same
    // repo -- its absence below is the filter working, not a fixture hole.
    const found = (await api.find("unrelated")) as { total: number };
    assert.equal(found.total, 1, "premise: the bystander entity exists and is indexed");

    const receipt = (await api.edit({
      file: "core.ts",
      entity: { name: "target" },
      op: "replace",
      content: "export function target(): number {\n  return 2;\n}",
    })) as { impact: string; dependents_before: Array<{ name: string }> };

    assert.equal(receipt.impact, "1 caller (caller1)");
    assert.doesNotMatch(receipt.impact, /unrelated/);
    assert.deepEqual(
      receipt.dependents_before.map((d) => d.name),
      ["caller1"],
      "dependents_before is exactly the set of referencing entities",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

/** Async poll for a signal file -- must not block the event loop, because the engine under test runs in THIS process. */
async function waitForFile(path: string, label: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  while (!existsSync(path)) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${label}`);
    await new Promise((r) => setTimeout(r, 25));
  }
}

test("(b, live) real weave-mcp: mid-flight drift is merged by the real engine and the receipt's merge claims match disk", LIVE_SKIP, async () => {
  const dir = makeRepo();
  const signals = mkdtempSync(join(tmpdir(), "law-receipt-signals-"));
  const coordinator = new Coordinator({ command: WEAVE_MCP!, cwd: dir, agentId: "law-receipt-live-agent" });
  const savedEnv = { ...process.env };
  try {
    // Gate the FIRST `sem entities` call (which happens AFTER the engine's
    // own base-content read), and inject the concurrent beta change while
    // it is blocked -- real drift, deterministically, no sleeps.
    process.env.LAWS_GATED_DIR = signals;
    process.env.LAWS_GATED_REAL = "sem";
    process.env.LAWS_GATE_SUBCOMMAND = "entities";
    process.env.LAWS_GATE_NTH = "1";
    process.env.LAWS_GATE_STYLE = "pre";

    const pending = performWeaveEdit(
      { file: "calc.ts", entity: { name: "alpha" }, op: "replace", content: ALPHA_OURS },
      { cwd: dir, semBin: LAWS_GATED_SEM, coordinator },
    );
    await waitForFile(join(signals, "blocked-pre"), "engine to reach its gated extraction");
    writeFileSync(join(dir, "calc.ts"), FILE_V0.replace(BETA, BETA_THEIRS)); // the concurrent writer
    writeFileSync(join(signals, "continue-pre"), "");

    const outcome: WeaveEditOutcome = await pending;
    assert.equal(outcome.isError, false, outcome.text);
    const merge = outcome.details.merge as MergeStatus;
    assert.equal(merge.attempted, true);
    assert.equal(merge.performed, true, "non-vacuity: the real engine must actually have detected and merged the drift");
    assert.ok((merge.mergedOver ?? []).includes("beta"), `mergedOver names the drifted entity, got ${JSON.stringify(merge.mergedOver)}`);
    assert.ok(!(merge.mergedOver ?? []).includes("alpha"), "this edit's own entity is filtered out of mergedOver");
    const disk = readFileSync(join(dir, "calc.ts"), "utf8");
    assert.ok(disk.includes("x + 42"), "receipt says our edit landed -- disk agrees");
    assert.ok(disk.includes("y * 9"), "receipt says the merge carried theirs -- disk agrees");
  } finally {
    process.env = savedEnv;
    await coordinator.stop();
    rmSync(dir, { recursive: true, force: true });
    rmSync(signals, { recursive: true, force: true });
  }
});
