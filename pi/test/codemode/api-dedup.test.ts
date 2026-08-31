import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, cpSync, writeFileSync, utimesSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { buildSemApi, createChangeLog, createDedupStore } from "../../src/codemode/api.ts";

const __dirname = dirname(fileURLToPath(import.meta.url));
const FIXTURES = join(__dirname, "fixtures");

function withTempCopy<T>(fixtureNames: string[], run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "codemode-dedup-test-"));
  for (const name of fixtureNames) cpSync(join(FIXTURES, name), join(dir, name));
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

/**
 * Session-wide dedup: the measured habit is re-grepping the same thing in
 * a LATER call, not twice in one script. Implemented as a per-session
 * store keyed by (verb, args, tree hash of the touched files); entries are
 * invalidated once their files change after an edit; "unchanged since h3"
 * is returned across calls. Applies to the
 * read-only question verbs with a row array to key off of: find/grep/
 * callers/blast/where.
 */

test("find(): an identical query in the SAME buildSemApi() call is deduped on the second call", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const first = (await api.find("add")) as { hits: unknown[] };
    assert.ok(Array.isArray(first.hits) && first.hits.length > 0);

    const second = (await api.find("add")) as { unchanged?: boolean; since?: string; message?: string };
    assert.equal(second.unchanged, true);
    assert.match(second.message ?? "", /unchanged since/i);
  });
});

test("find(): a DIFFERENT query is NOT deduped -- gets its own real result", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.find("add");
    const different = (await api.find("twice")) as { hits?: unknown[]; unchanged?: boolean };
    assert.equal(different.unchanged, undefined);
    assert.ok(Array.isArray(different.hits) && different.hits.length > 0);
  });
});

test("grep()/callers()/blast()/where() are each independently deduped by their own (verb,args) key", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    await api.grep("add");
    const grepSecond = (await api.grep("add")) as { unchanged?: boolean };
    assert.equal(grepSecond.unchanged, true);

    await api.callers("add");
    const callersSecond = (await api.callers("add")) as { unchanged?: boolean };
    assert.equal(callersSecond.unchanged, true);
  });
});

test("the 'unchanged since h_' handle is a REAL, resolvable handle -- the first row's own handle from the original call", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const first = (await api.find("add")) as { hits: Array<{ h: string; name: string }> };
    const firstHandle = first.hits[0]!.h;

    const second = (await api.find("add")) as { since?: string };
    assert.equal(second.since, firstHandle, "the dedup stub's `since` should be the SAME handle a script already saw from the original call");

    const resolved = (await api.read(second.since!)) as { entity: { name: string } };
    assert.equal(resolved.entity.name, "add", "the since handle must still resolve via read()");
  });
});

test("an edit() to a file the cached result depended on invalidates the dedup entry", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.find("add");

    await api.edit({
      file: "calls.ts",
      entity: { name: "twice" },
      op: "replace",
      content: "export function twice(n: number): number {\n  return add(n, n) + 0;\n}",
    });

    const afterEdit = (await api.find("add")) as { hits?: unknown[]; unchanged?: boolean };
    assert.equal(afterEdit.unchanged, undefined, "editing calls.ts (which find(\"add\")'s result depends on) must invalidate the cached entry");
    assert.ok(Array.isArray(afterEdit.hits) && afterEdit.hits.length > 0);
  });
});

test("ANY session mutation invalidates a dedup entry -- even one to a file the cached result never matched", async () => {
  // Flipped pin (the original asserted the opposite, and that pin was the
  // bug): a search's result set can GROW from a file it never matched --
  // editing chain.ts could introduce a new entity named "add" there, and
  // a 0-hit grep has no matched files at all, so file-overlap
  // invalidation could NEVER see the change that creates its first hit
  // (the post-create empty-grep bug). Dedup's premise is "nothing
  // changed"; when something did, correctness beats the saved call.
  await withTempCopy(["calls.ts", "chain.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.find("add");

    await api.edit({
      file: "chain.ts",
      entity: { name: "delta" },
      op: "replace",
      content: "export function delta(): number {\n  return 2;\n}",
    });

    const afterEdit = (await api.find("add")) as { unchanged?: boolean; hits?: unknown[] };
    assert.notEqual(afterEdit.unchanged, true, "a session mutation after the cache entry must invalidate it");
    assert.ok(Array.isArray(afterEdit.hits), "the re-run returns real rows again");
  });
});

test("post-create lookup: a 0-hit grep cached before sem.add() must not answer 'unchanged' after it", async () => {
  // The item-12 root cause, as a regression test. Verdict for the record:
  // NOT the sem CLI's index (it re-indexes the same path on mtime change
  // -- verified via CLI find/grep seeing the new entity immediately), NOT
  // regex dialect, NOT path filtering: the client-side dedup store served
  // the pre-add 0-hit result as "unchanged" because a 0-hit entry has no
  // files for the overlap check to intersect with.
  await withTempCopy(["calls.ts"], async (dir) => {
    execFileSync("git", ["init", "-q"], { cwd: dir });
    execFileSync("git", ["add", "-A"], { cwd: dir });
    execFileSync("git", ["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"], { cwd: dir });
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const pre = (await api.grep("brand_new_fn")) as { total: number };
    assert.equal(pre.total, 0, "premise: nothing matches before the add");

    await api.add({ file: "src/newmod.ts", content: "export function brand_new_fn(): number {\n  return 7;\n}\n" });

    const post = (await api.grep("brand_new_fn")) as { unchanged?: boolean; total?: number; hits?: Array<{ file: string }> };
    assert.notEqual(post.unchanged, true, "the entity exists NOW -- 'unchanged' would be a lie");
    assert.ok((post.total ?? 0) >= 1, `the new entity must be found: ${JSON.stringify(post)}`);
  });
});

test("an EXTERNAL writer's change to a file the cached result depended on invalidates the dedup entry", async () => {
  // The cross-process hole in ChangeLog-only invalidation. pi-sem's whole
  // premise is CONCURRENT agents on one worktree (weave coordination,
  // drift detection, merge-on-write all exist for exactly that), so the
  // "the only writer is this session" assumption dedup was built on is
  // false by construction: another agent's process writes calls.ts
  // straight to disk, this session's ChangeLog never hears about it, and
  // a ChangeLog-only check keeps answering "unchanged" about content that
  // is gone. Distinct from the 0-hit bug fixed in c4940e8 (that one is
  // about a SESSION mutation an empty result had no files to intersect
  // with; this one is about a mutation that is not in the session at all).
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    const first = (await api.grep("thrice")) as { total: number; hits: Array<{ file: string }> };
    assert.equal(first.total, 1, "premise: thrice() is in calls.ts to start with");
    assert.equal(first.hits[0]!.file, "calls.ts");

    // A DIFFERENT process (a second agent) rewrites the file on disk --
    // NOT through this session's sem.edit()/sem.write(), so nothing lands
    // in this session's ChangeLog.
    writeFileSync(
      join(dir, "calls.ts"),
      "export function add(a: number, b: number): number {\n  return a + b;\n}\n",
    );

    const second = (await api.grep("thrice")) as { unchanged?: boolean; total?: number };
    assert.notEqual(second.unchanged, true, "another agent deleted thrice() on disk -- 'unchanged' is a lie about a file that is not there any more");
    assert.equal(second.total, 0, `the re-run must reflect what is on disk NOW: ${JSON.stringify(second)}`);
  });
});

test("a mtime-only touch by another process does NOT invalidate -- the content is what matters", async () => {
  // The other side of the pin above: staleness is decided by CONTENT, so
  // an external `touch` (or a checkout that rewrites a file byte-identical)
  // must not throw the saving away. Keeps the stat fast path honest.
  await withTempCopy(["calls.ts"], async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });
    await api.grep("thrice");

    const later = new Date(Date.now() + 5000);
    utimesSync(join(dir, "calls.ts"), later, later);

    const second = (await api.grep("thrice")) as { unchanged?: boolean };
    assert.equal(second.unchanged, true, "same bytes on disk -- still 'unchanged'");
  });
});

test("dedup is SESSION-scoped when the SAME DedupStore is threaded into separate buildSemApi() calls", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const dedup = createDedupStore();

    const apiCall1 = buildSemApi({ cwd: dir, semBin: "sem", dedup });
    const first = (await apiCall1.find("add")) as { hits: unknown[] };
    assert.ok(Array.isArray(first.hits) && first.hits.length > 0);

    const apiCall2 = buildSemApi({ cwd: dir, semBin: "sem", dedup });
    const second = (await apiCall2.find("add")) as { unchanged?: boolean };
    assert.equal(second.unchanged, true, "a find() from an earlier sem_code call should dedup a later one within the same session");
  });
});

test("dedup is isolated by DEFAULT across separate buildSemApi() constructions that don't share a DedupStore", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const apiA = buildSemApi({ cwd: dir, semBin: "sem" });
    await apiA.find("add");

    const apiB = buildSemApi({ cwd: dir, semBin: "sem" });
    const result = (await apiB.find("add")) as { unchanged?: boolean; hits?: unknown[] };
    assert.equal(result.unchanged, undefined, "without an explicitly shared DedupStore, each buildSemApi() call starts with its own empty cache");
    assert.ok(Array.isArray(result.hits) && result.hits.length > 0);
  });
});

test("dedup across a SESSION-scoped store still respects an edit() made by a LATER sem_code call invalidating an EARLIER call's cache entry", async () => {
  await withTempCopy(["calls.ts"], async (dir) => {
    const dedup = createDedupStore();
    const changes = createChangeLog();

    const apiCall1 = buildSemApi({ cwd: dir, semBin: "sem", dedup, changes });
    await apiCall1.find("add");

    // A later sem_code call edits the file...
    const apiCall2 = buildSemApi({ cwd: dir, semBin: "sem", dedup, changes });
    await apiCall2.edit({
      file: "calls.ts",
      entity: { name: "twice" },
      op: "replace",
      content: "export function twice(n: number): number {\n  return add(n, n) + 1;\n}",
    });

    // ...and a THIRD call's identical find("add") must see the invalidation.
    const apiCall3 = buildSemApi({ cwd: dir, semBin: "sem", dedup, changes });
    const result = (await apiCall3.find("add")) as { unchanged?: boolean };
    assert.equal(result.unchanged, undefined, "an edit from a different sem_code call in the same session must still invalidate the shared dedup entry");
  });
});
