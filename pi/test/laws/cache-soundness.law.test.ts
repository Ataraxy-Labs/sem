// LAW 3 -- CACHE SOUNDNESS (the dedup law).
//
// STRUCTURE: codemode's dedup (src/codemode/api.ts, withDedup/DedupStore)
// is MEMOIZATION over (verb, args) with CONTENT-ADDRESSED invalidation:
// each cached answer carries stamps (size, mtimeMs, sha1) of the files in
// its result rows -- its recorded SUPPORT -- and is served only when (i)
// no session mutation happened since (the ChangeLog arm) and (ii) every
// stamped file is byte-identical to stamp time (stat-then-hash; content
// decides, never the clock). The law that HOLDS:
//
//     served-from-cache  =>  every STAMPED file is byte-identical
//                            AND no session write intervened.
//
// What this is NOT (the NOT-THAT half, witnessed as a documented domain
// condition rather than left implicit): the stamps record only the
// POSITIVE support of the answer. A negative or grown answer depends on
// files outside that set -- a 0-hit result stamps nothing at all, and any
// result can grow a new row from a file it never matched. Cross-process
// writers in that complement are invisible by construction (the in-session
// half of the same hole IS closed, by the ChangeLog arm -- witnessed in
// test/codemode/api-dedup.test.ts's post-create tests, which this file
// builds on and does not duplicate). So "unchanged since h_" means
// "support unchanged", not "answer identical" -- stamp-soundness, not
// referential transparency.
//
// Non-vacuity probes: the support-coverage test mutates EACH file of a
// multi-file support in turn and demands a cache miss for every one (the
// coverage floor: no stamped file is dead weight), and the boundary tests
// first prove the external write really does change the TRUE answer (a
// fresh, differently-keyed query sees it) before asserting the cache still
// answers "unchanged" -- so the characterization can never pass vacuously.
import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { mkdtempSync, rmSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { buildSemApi, createChangeLog, createDedupStore } from "../../src/codemode/api.ts";

function makeDir(files: Record<string, string>): string {
  const dir = mkdtempSync(join(tmpdir(), "law-cache-"));
  for (const [name, content] of Object.entries(files)) writeFileSync(join(dir, name), content);
  return dir;
}

const A_TS = "export function alphaThing(): number {\n  return needle_token() + 1;\n}\nfunction needle_token(): number {\n  return 0;\n}\n";
const B_TS = "export function betaThing(): number {\n  return needle_token2();\n}\nfunction needle_token2(): number {\n  return 0;\n}\n";

interface GrepResult {
  unchanged?: boolean;
  total?: number;
  hits?: Array<{ file: string }>;
}

test("serve-condition: an identical call is served from cache ONLY while every stamped support file is byte-identical -- each file of a multi-file support invalidates on its own", async () => {
  const dir = makeDir({ "a.ts": A_TS.replace("needle_token2", "x"), "b.ts": B_TS.replace("needle_token2", "shared_needle") });
  try {
    writeFileSync(join(dir, "a.ts"), "export function alphaThing(): number {\n  return shared_needle();\n}\n");
    writeFileSync(join(dir, "b.ts"), "export function betaThing(): number {\n  return shared_needle();\n}\nfunction shared_needle(): number {\n  return 0;\n}\n");
    const api = buildSemApi({ cwd: dir, semBin: "sem", dedup: createDedupStore(), changes: createChangeLog() });

    const first = (await api.grep("shared_needle")) as GrepResult;
    const supportFiles = [...new Set((first.hits ?? []).map((h) => (h.file.startsWith("@") ? h.file.slice(1) : h.file)))];
    assert.ok(supportFiles.length >= 2, `non-vacuity: the support must span multiple files, got ${JSON.stringify(supportFiles)}`);

    // Unchanged support -> served from cache.
    const second = (await api.grep("shared_needle")) as GrepResult;
    assert.equal(second.unchanged, true, "identical call over identical bytes is the memoization hit");

    // Coverage floor: EVERY stamped file's content is live -- mutating any
    // single one of them must break the serve condition.
    for (const file of supportFiles) {
      const path = join(dir, file);
      const before = readFileSync(path, "utf8");
      writeFileSync(path, `${before}// external writer touched ${file}\n`);
      const after = (await api.grep("shared_needle")) as GrepResult;
      assert.notEqual(after.unchanged, true, `external content change to ${file} must invalidate the cached answer`);
      // Re-prime the cache for the next file's probe.
      const reprime = (await api.grep("shared_needle")) as GrepResult;
      assert.equal(reprime.unchanged, true, "cache re-primes after the miss");
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("content decides, never the clock: a byte-identical rewrite (fresh mtime) still serves from cache -- the hash settles what stat cannot", async () => {
  const dir = makeDir({ "a.ts": A_TS });
  try {
    const api = buildSemApi({ cwd: dir, semBin: "sem", dedup: createDedupStore(), changes: createChangeLog() });
    const first = (await api.grep("needle_token")) as GrepResult;
    assert.ok((first.hits ?? []).length > 0, "non-vacuity: the query matches");

    await new Promise((r) => setTimeout(r, 10)); // guarantee a different mtimeMs
    writeFileSync(join(dir, "a.ts"), A_TS); // same bytes, new timestamp

    const second = (await api.grep("needle_token")) as GrepResult;
    assert.equal(second.unchanged, true, "byte-identical content must serve from cache regardless of mtime -- staleness is decided by CONTENT");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("documented domain condition (0-hit boundary): a 0-hit answer stamps nothing, so a CROSS-PROCESS writer creating the first match is invisible to it", async () => {
  const dir = makeDir({ "a.ts": A_TS });
  try {
    const api = buildSemApi({ cwd: dir, semBin: "sem", dedup: createDedupStore(), changes: createChangeLog() });
    const pre = (await api.grep("phantom_symbol")) as GrepResult;
    assert.equal(pre.total, 0, "premise: nothing matches yet");

    // An external process (not this session -- no ChangeLog entry) creates
    // the first match.
    writeFileSync(join(dir, "newcomer.ts"), "export function phantom_symbol(): number {\n  return 1;\n}\n");

    // Non-vacuity: the TRUE answer has changed -- a differently-keyed query
    // sees the new match immediately, so any staleness below is the
    // cache's, not the search engine's.
    const differentKey = (await api.grep("phantom_symbol", { glob: "*.ts" })) as GrepResult;
    assert.ok((differentKey.total ?? 0) > 0, "the new match is real and findable");

    // The boundary itself: the identical call still answers from cache --
    // an empty stamp set has nothing to go stale. This is the disclosed
    // domain condition (see DedupStore's doc comment), pinned here so the
    // day it changes, this characterization changes with it.
    const post = (await api.grep("phantom_symbol")) as GrepResult;
    assert.equal(post.unchanged, true, "DOCUMENTED BOUNDARY: 0-hit answers cannot see cross-process growth (their support set is empty)");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("documented domain condition (growth boundary, generalized): cross-process growth from OUTSIDE the stamped support is equally invisible to a non-empty answer", async () => {
  const dir = makeDir({ "a.ts": A_TS });
  try {
    const api = buildSemApi({ cwd: dir, semBin: "sem", dedup: createDedupStore(), changes: createChangeLog() });
    const first = (await api.grep("needle_token")) as GrepResult;
    const firstTotal = first.total ?? 0;
    assert.ok(firstTotal > 0, "premise: a non-empty answer, with a non-empty stamp set");

    // External writer adds a match in a file the cached answer never saw.
    writeFileSync(join(dir, "elsewhere.ts"), "export function other(): number {\n  return needle_token_extra();\n}\nfunction needle_token_extra(): number {\n  return 0;\n}\n");

    const differentKey = (await api.grep("needle_token", { glob: "*.ts" })) as GrepResult;
    assert.ok((differentKey.total ?? 0) > firstTotal, "non-vacuity: the true answer really grew");

    const post = (await api.grep("needle_token")) as GrepResult;
    assert.equal(post.unchanged, true, "DOCUMENTED BOUNDARY: stamps witness change of existing support, never growth from outside it");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the ChangeLog arm closes the SAME hole in-session: a session write after a 0-hit cache entry invalidates it, even with an empty stamp set", async () => {
  const dir = makeDir({ "a.ts": A_TS });
  try {
    const api = buildSemApi({ cwd: dir, semBin: "sem", dedup: createDedupStore(), changes: createChangeLog() });
    const pre = (await api.grep("phantom_symbol")) as GrepResult;
    assert.equal(pre.total, 0, "premise: 0-hit, empty stamp set");

    await api.write("newcomer.ts", "export function phantom_symbol(): number {\n  return 1;\n}\n");

    const post = (await api.grep("phantom_symbol")) as GrepResult;
    assert.notEqual(post.unchanged, true, "any session mutation after the entry invalidates it -- the arm the stamps cannot provide");
    assert.ok((post.total ?? 0) > 0, "the re-run sees the in-session creation");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
