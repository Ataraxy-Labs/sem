// The write-window regression: two agents as two real OS processes, free
// running (no gated-sem seam, no signal files, no forced interleave),
// editing DISJOINT entities in the SAME file as fast as they can. This is
// the case the merge backstop's gate cannot decide on its own -- its drift
// verdict is computed from a disk read one MCP round trip before the local
// write, so a second process's write landing in that gap is invisible to
// the verdict that is about to overwrite it.
//
// Measured on this engine before the bounded retry in performOneWeaveEdit
// existed: 5/5 runs of 20 edits per agent silently lost at least one edit
// (11 of 200 edits gone with claims on, 20 of 200 with claims off) -- no
// error, no conflict, no merge, just an entity whose own edit is absent
// from the final file. Not a rare edge case; the common case for two fast
// agents sharing a file.
//
// The invariant asserted here is the one that matters and the only one
// that is deterministic under a real race: every edit either lands on disk
// or comes back as an ERROR. Silent loss -- an edit reported successful
// whose text is not in the file -- is the regression. Merge counts, retry
// counts and which agent wins any particular entity are all legitimately
// timing-dependent and are deliberately not asserted.
//
// Needs a real weave-mcp (PI_SEM_WEAVE_MCP_BIN or PATH) and the real sem
// CLI; skips with a named reason otherwise. The deterministic, sequenced
// sibling of this race lives in weave-coordination-cross-process.test.ts.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { execFileSync, spawn, spawnSync } from "node:child_process";

const __dirname = dirname(fileURLToPath(import.meta.url));
const DRIVER = join(__dirname, "fixtures", "coord-e2e-driver.mjs");

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

/** Edits per agent. Enough round trips for the two processes to drift into and out of phase repeatedly; the pre-fix loss rate at this size was 100% of runs. */
const EDITS_PER_AGENT = 10;

/** Each agent's own marker, so an entity's edit is identifiable in the final file no matter who merged it in. */
const MARKER = { a: 2000, b: 3000 };

function makeRepo(nFunctions: number): string {
  const dir = mkdtempSync(join(tmpdir(), "weave-write-window-"));
  const fns: string[] = [];
  for (let i = 0; i < nFunctions; i++) fns.push(`export function f${i}(x: number): number {\n  return x + ${i};\n}`);
  writeFileSync(join(dir, "batch.ts"), fns.join("\n\n") + "\n");
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

/** Every even index for agent A, every odd index for agent B -- disjoint entities, one shared file. */
function editsFor(agent: "a" | "b"): Array<{ index: number; params: unknown }> {
  const out: Array<{ index: number; params: unknown }> = [];
  for (let i = 0; i < EDITS_PER_AGENT; i++) {
    const index = agent === "a" ? i * 2 : i * 2 + 1;
    out.push({
      index,
      params: {
        file: "batch.ts",
        entity: { name: `f${index}` },
        op: "replace",
        content: `export function f${index}(x: number): number {\n  return x + ${index} + ${MARKER[agent]};\n}`,
      },
    });
  }
  return out;
}

function spawnAgent(cwd: string, agentId: string, paramsList: unknown[]): Promise<DriverOutcome[]> {
  const spec = JSON.stringify({ cwd, agentId, weaveMcp: WEAVE_MCP, semBin: SEM, paramsList });
  const child = spawn(process.execPath, [DRIVER, spec], { stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "";
  let stderr = "";
  child.stdout!.on("data", (c: Buffer) => (stdout += c.toString()));
  child.stderr!.on("data", (c: Buffer) => (stderr += c.toString()));
  return new Promise<DriverOutcome[]>((resolvePromise, reject) => {
    child.on("close", () => {
      try {
        resolvePromise(JSON.parse(stdout) as DriverOutcome[]);
      } catch {
        reject(new Error(`driver ${agentId} produced no outcome JSON.\nstdout: ${stdout}\nstderr: ${stderr}`));
      }
    });
    child.on("error", reject);
  });
}

test("free-running cross-process race on disjoint entities: no edit is ever lost silently", SKIP, async () => {
  const dir = makeRepo(EDITS_PER_AGENT * 2);
  try {
    const editsA = editsFor("a");
    const editsB = editsFor("b");
    const [outcomesA, outcomesB] = await Promise.all([
      spawnAgent(dir, "race-agent-a", editsA.map((e) => e.params)),
      spawnAgent(dir, "race-agent-b", editsB.map((e) => e.params)),
    ]);

    assert.equal(outcomesA.length, editsA.length, "agent A ran every edit");
    assert.equal(outcomesB.length, editsB.length, "agent B ran every edit");

    const disk = readFileSync(join(dir, "batch.ts"), "utf8");
    const silentlyLost: string[] = [];
    const reportedFailures: string[] = [];
    for (const [edits, outcomes, agent] of [
      [editsA, outcomesA, "a"],
      [editsB, outcomesB, "b"],
    ] as const) {
      edits.forEach(({ index }, i) => {
        const landed = disk.includes(`x + ${index} + ${MARKER[agent]};`);
        const outcome = outcomes[i]!;
        if (landed) return;
        // Absent from disk is only acceptable if the tool SAID so. A
        // bounded-retry refusal, a same-entity conflict and a rolled-back
        // verification failure all qualify: the caller knows the edit did
        // not land and can retry. A success whose text is missing does not.
        if (outcome.isError) reportedFailures.push(`f${index}: ${outcome.text.split("\n")[0]}`);
        else silentlyLost.push(`f${index}`);
      });
    }

    assert.deepEqual(
      silentlyLost,
      [],
      `every edit reported successful must be on disk. Missing without any error: ${silentlyLost.join(", ")}.\nReported failures (acceptable): ${reportedFailures.join(" | ") || "none"}\nDisk:\n${disk}`,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
