// The cross-process coordination WITNESS: two agents as two real OS
// processes (coord-e2e-driver.mjs), each with its own Coordinator and its
// own weave-mcp child, sharing nothing but the repo directory -- disk plus
// .weave CRDT state -- exactly like two real agents on one checkout. The
// interleaving is deterministic via the gated-sem seam (see gated-sem.mjs:
// agent A is held between its file read and its merge gate while agent B's
// edit fully lands), driven by signal FILES, never sleeps-for-sequencing.
//
// Needs a real weave-mcp (PI_SEM_WEAVE_MCP_BIN or PATH) and the real sem
// CLI; skips with a named reason otherwise. The in-process deterministic
// coverage of the same verdicts lives in weave-edit-merge-backstop.test.ts.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, mkdirSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import type { ChildProcess } from "node:child_process";

const __dirname = dirname(fileURLToPath(import.meta.url));
const DRIVER = join(__dirname, "fixtures", "coord-e2e-driver.mjs");
const GATED_SEM = join(__dirname, "fixtures", "gated-sem.mjs");

function resolveWeaveMcp(): string | undefined {
  const fromEnv = process.env.PI_SEM_WEAVE_MCP_BIN;
  if (fromEnv && spawnSync(fromEnv, ["--version"], { stdio: "ignore" }).error === undefined) return fromEnv;
  if (spawnSync("weave-mcp", ["--version"], { stdio: "ignore" }).error === undefined) return "weave-mcp";
  return undefined;
}

function resolveSem(): string | undefined {
  return spawnSync("sem", ["--version"], { stdio: "ignore" }).error === undefined ? "sem" : undefined;
}

const WEAVE_MCP = resolveWeaveMcp();
const SEM = resolveSem();
const SKIP =
  WEAVE_MCP === undefined
    ? { skip: "no weave-mcp binary (set PI_SEM_WEAVE_MCP_BIN or put weave-mcp on PATH)" }
    : SEM === undefined
      ? { skip: "no sem CLI on PATH" }
      : {};

const GREET_V0 = "export function greet(name: string): string {\n  return `Hello, ${name}!`;\n}";
const GREET_A = "export function greet(name: string): string {\n  return `Hi, ${name}!`;\n}";
const GREET_B = "export function greet(name: string): string {\n  return `Yo, ${name}!`;\n}";
const FAREWELL_V0 = "export function farewell(name: string): string {\n  return `Bye, ${name}.`;\n}";
const FAREWELL_B = "export function farewell(name: string): string {\n  return `Farewell, ${name}.`;\n}";

function makeRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "coord-e2e-"));
  writeFileSync(join(dir, "hello.ts"), `${GREET_V0}\n\n${FAREWELL_V0}\n`);
  execFileSync("git", ["init", "-q", "-b", "main"], { cwd: dir });
  execFileSync("git", ["add", "-A"], { cwd: dir });
  execFileSync("git", ["-c", "user.email=t@t.com", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
  return dir;
}

interface DriverOutcome {
  isError: boolean;
  text: string;
  details: Record<string, unknown>;
}

function spawnAgent(cwd: string, agentId: string, params: unknown, semBin: string, extraEnv: Record<string, string> = {}): ChildProcess & { outcome: Promise<DriverOutcome> } {
  const spec = JSON.stringify({ cwd, agentId, weaveMcp: WEAVE_MCP, semBin, params });
  const child = spawn(process.execPath, [DRIVER, spec], { env: { ...process.env, ...extraEnv }, stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "";
  let stderr = "";
  child.stdout!.on("data", (c: Buffer) => (stdout += c.toString()));
  child.stderr!.on("data", (c: Buffer) => (stderr += c.toString()));
  const outcome = new Promise<DriverOutcome>((resolvePromise, reject) => {
    child.on("close", () => {
      try {
        resolvePromise(JSON.parse(stdout) as DriverOutcome);
      } catch {
        reject(new Error(`driver ${agentId} produced no outcome JSON.\nstdout: ${stdout}\nstderr: ${stderr}`));
      }
    });
    child.on("error", reject);
  });
  return Object.assign(child, { outcome });
}

async function waitForFile(path: string, ms = 30_000): Promise<void> {
  const deadline = Date.now() + ms;
  while (!existsSync(path)) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for signal file ${path}`);
    await new Promise((r) => setTimeout(r, 25));
  }
}

test("cross-process disjoint edits: B lands mid-flight, A's backstop merges over it -- both edits survive on disk", SKIP, async () => {
  const dir = makeRepo();
  const signals = join(dir, ".signals");
  mkdirSync(signals);
  try {
    // Agent A edits greet, held by the gated sem between its read and its
    // merge gate.
    const agentA = spawnAgent(
      dir,
      "witness-agent-a",
      { file: "hello.ts", entity: { name: "greet" }, op: "replace", content: GREET_A },
      GATED_SEM,
      { GATED_SEM_SIGNAL_DIR: signals, GATED_SEM_REAL: SEM! },
    );
    await waitForFile(join(signals, "blocked"));

    // Agent B (real sem, separate process, separate weave-mcp) fully lands
    // a FAREWELL edit while A is holding its stale read.
    const agentB = spawnAgent(dir, "witness-agent-b", { file: "hello.ts", entity: { name: "farewell" }, op: "replace", content: FAREWELL_B }, SEM!);
    const outcomeB = await agentB.outcome;
    assert.equal(outcomeB.isError, false, outcomeB.text);
    assert.ok(readFileSync(join(dir, "hello.ts"), "utf8").includes("Farewell,"), "B's edit is on disk before A resumes");

    // Release A.
    writeFileSync(join(signals, "continue"), "");
    const outcomeA = await agentA.outcome;
    assert.equal(outcomeA.isError, false, outcomeA.text);

    const disk = readFileSync(join(dir, "hello.ts"), "utf8");
    assert.ok(disk.includes("Hi, ${name}!"), "A's greet edit survived");
    assert.ok(disk.includes("Farewell,"), "B's farewell edit was merged over, not clobbered by A's stale snapshot");
    const merge = outcomeA.details.merge as { performed: boolean; driftDetected?: boolean; mergedOver?: string[] };
    assert.equal(merge.performed, true, `A's backstop should have performed a merge; got ${JSON.stringify(merge)}`);
    assert.equal(merge.driftDetected, true);
    assert.ok((merge.mergedOver ?? []).includes("farewell"), `mergedOver should name farewell: ${JSON.stringify(merge.mergedOver)}`);
    assert.match(outcomeA.text, /Merged over concurrent changes to farewell/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("cross-process same-entity collision: A is refused, B's version stands -- no silent last-writer-wins", SKIP, async () => {
  const dir = makeRepo();
  const signals = join(dir, ".signals");
  mkdirSync(signals);
  try {
    const agentA = spawnAgent(
      dir,
      "witness-agent-a2",
      { file: "hello.ts", entity: { name: "greet" }, op: "replace", content: GREET_A },
      GATED_SEM,
      { GATED_SEM_SIGNAL_DIR: signals, GATED_SEM_REAL: SEM! },
    );
    await waitForFile(join(signals, "blocked"));

    // B rewrites the SAME entity while A holds its stale read. B's own
    // claim may lose to A's (advisory -- B proceeds); B's own gate sees no
    // drift (disk is still v0 from B's fresh read) and lands.
    const agentB = spawnAgent(dir, "witness-agent-b2", { file: "hello.ts", entity: { name: "greet" }, op: "replace", content: GREET_B }, SEM!);
    const outcomeB = await agentB.outcome;
    assert.equal(outcomeB.isError, false, outcomeB.text);

    writeFileSync(join(signals, "continue"), "");
    const outcomeA = await agentA.outcome;

    assert.equal(outcomeA.isError, true, `A must be refused, not silently clobber B: ${outcomeA.text}`);
    assert.match(outcomeA.text, /collides|refused/);
    const disk = readFileSync(join(dir, "hello.ts"), "utf8");
    assert.ok(disk.includes("Yo, ${name}!"), "B's version of greet stands");
    assert.ok(!disk.includes("Hi, ${name}!"), "A's refused version must not be on disk");
    const merge = outcomeA.details.merge as { attempted: boolean; performed: boolean; driftDetected?: boolean };
    assert.deepEqual(merge, { attempted: true, performed: false, driftDetected: true });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
