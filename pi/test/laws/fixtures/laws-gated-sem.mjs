#!/usr/bin/env node
// A generalized "gated sem" seam for the law witnesses in test/laws/ --
// same idea as test/tools/fixtures/gated-sem.mjs (which gates only the
// FIRST invocation), but parameterized so a law test can interpose at any
// chosen point of performOneWeaveEdit's sem-subprocess sequence:
//
//   LAWS_GATED_DIR         signal directory (required)
//   LAWS_GATED_REAL        real sem binary (default "sem")
//   LAWS_GATE_SUBCOMMAND   which sem subcommand to count (default "entities")
//   LAWS_GATE_NTH          1-based index among matching invocations to gate
//   LAWS_GATE_STYLE        "pre"  -- block BEFORE running real sem (lets the
//                                    test mutate disk so this extraction
//                                    reads post-mutation bytes);
//                          "mid"  -- run real sem first, capture its output,
//                                    then block; emit the CAPTURED (now
//                                    stale) output only after the test
//                                    signals (lets the test mutate disk so
//                                    the CALLER acts on a stale extraction
//                                    -- the deterministic stand-in for
//                                    "another process wrote while sem's
//                                    read was in flight");
//                          "both" -- pre-block, run+capture, mid-block, emit.
//
// Signal protocol (all files inside LAWS_GATED_DIR):
//   blocked-pre    written here when pre-blocking starts
//   continue-pre   written by the test; unblocks the pre gate
//   extracted      written here after real sem ran and its output is captured
//   continue-post  written by the test; releases the captured output
//
// Non-gated invocations (wrong subcommand, or not the Nth) exec the real
// sem transparently. The invocation counter lives in a file per
// subcommand; performOneWeaveEdit runs its sem calls sequentially inside
// withFileMutationQueue, so the counter needs no cross-process locking in
// these single-engine tests.
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { join } from "node:path";

const napBuffer = new Int32Array(new SharedArrayBuffer(4));
const nap = (ms) => Atomics.wait(napBuffer, 0, 0, ms);

const dir = process.env.LAWS_GATED_DIR;
const realSem = process.env.LAWS_GATED_REAL ?? "sem";
const gateSub = process.env.LAWS_GATE_SUBCOMMAND ?? "entities";
const gateNth = Number(process.env.LAWS_GATE_NTH ?? "1");
const style = process.env.LAWS_GATE_STYLE ?? "pre";

if (!dir) {
  process.stderr.write("laws-gated-sem: LAWS_GATED_DIR not set\n");
  process.exit(1);
}

const args = process.argv.slice(2);
const sub = args[0];

function passThrough() {
  const result = spawnSync(realSem, args, { stdio: "inherit" });
  process.exit(result.status ?? 1);
}

if (sub !== gateSub) passThrough();

const counterFile = join(dir, `counter-${gateSub}`);
const count = (existsSync(counterFile) ? Number(readFileSync(counterFile, "utf8")) : 0) + 1;
writeFileSync(counterFile, String(count));
if (count !== gateNth) passThrough();

function waitFor(name, label) {
  const target = join(dir, name);
  const deadline = Date.now() + 30_000;
  while (!existsSync(target)) {
    if (Date.now() > deadline) {
      process.stderr.write(`laws-gated-sem: timed out waiting for ${label}\n`);
      process.exit(1);
    }
    nap(25);
  }
}

if (style === "pre" || style === "both") {
  writeFileSync(join(dir, "blocked-pre"), "");
  waitFor("continue-pre", "continue-pre signal");
}

const result = spawnSync(realSem, args, { encoding: "utf8" });

if (style === "mid" || style === "both") {
  writeFileSync(join(dir, "extracted"), "");
  waitFor("continue-post", "continue-post signal");
}

process.stdout.write(result.stdout ?? "");
process.stderr.write(result.stderr ?? "");
process.exit(result.status ?? 1);
