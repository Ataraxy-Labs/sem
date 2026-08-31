#!/usr/bin/env node
// The "gated sem" seam for the cross-process coordination witness
// (weave-coordination-cross-process.test.ts): a stand-in `sem` binary that
// BLOCKS on its first invocation until the test says go, then (and on
// every later invocation) execs the real sem with the same arguments.
//
// Why this seam exists: performWeaveEdit reads the file and then calls
// sem (extractEntities) INSIDE its mutation queue -- there is no test
// hook between the read and the merge gate. Gating sem's first call
// holds agent A exactly there, with its read already taken, while agent
// B lands a real concurrent edit in another PROCESS. That makes the
// drift the merge backstop exists for, deterministically -- a
// signal-file driver, not sleeps.
//
// Protocol (dir = GATED_SEM_SIGNAL_DIR):
//   dir/blocked   -- written by this script when it starts blocking
//   dir/continue  -- written by the test; unblocks (and pre-unblocks
//                    every later invocation)
import { existsSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";

// Synchronous 25ms nap without burning CPU or forking: Atomics.wait on a
// throwaway shared buffer.
const napBuffer = new Int32Array(new SharedArrayBuffer(4));
const nap = (ms) => Atomics.wait(napBuffer, 0, 0, ms);

const dir = process.env.GATED_SEM_SIGNAL_DIR;
const realSem = process.env.GATED_SEM_REAL ?? "sem";
if (!dir) {
  process.stderr.write("gated-sem: GATED_SEM_SIGNAL_DIR not set\n");
  process.exit(1);
}

const continueFile = join(dir, "continue");
if (!existsSync(continueFile)) {
  writeFileSync(join(dir, "blocked"), "");
  // Bounded poll on the signal file -- 25ms granularity, 30s ceiling. The
  // ORDERING is file-driven (the test writes `continue` only after agent
  // B's edit has fully landed); the poll interval is just wakeup grain.
  const deadline = Date.now() + 30_000;
  while (!existsSync(continueFile)) {
    if (Date.now() > deadline) {
      process.stderr.write("gated-sem: timed out waiting for continue signal\n");
      process.exit(1);
    }
    nap(25);
  }
}

const result = spawnSync(realSem, process.argv.slice(2), { stdio: "inherit" });
process.exit(result.status ?? 1);
