import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildRoutinesPromptSection, buildSystemPromptAddendum, CODE_MODE_ADDENDUM } from "../../src/codemode/tool.ts";

/**
 * The probe's finding, turned into code: a fresh session can't answer
 * "done this before?" from its own (empty) memory, so the repo's memory is
 * SHOWN at session start -- a concrete listing of .sem/routines/*.mjs
 * injected into the code-mode addendum. Zero routines => zero prompt cost.
 */

function makeRepo(routines: Array<{ name: string; description?: string; params?: Record<string, unknown>; mtime?: number; raw?: string }>): string {
  const dir = mkdtempSync(join(tmpdir(), "sem-routines-prompt-"));
  const rdir = join(dir, ".sem", "routines");
  mkdirSync(rdir, { recursive: true });
  for (const r of routines) {
    const file = join(rdir, `${r.name}.mjs`);
    const header = r.raw ?? `// sem:routine ${JSON.stringify({ name: r.name, description: r.description ?? "", params: r.params ?? {}, created: "2026-08-27T00:00:00.000Z" })}`;
    writeFileSync(file, `${header}\nreturn 1;\n`);
    if (r.mtime !== undefined) utimesSync(file, r.mtime, r.mtime);
  }
  return dir;
}

test("no routines saved: no section at all -- the addendum is byte-identical to the static one", () => {
  const dir = mkdtempSync(join(tmpdir(), "sem-routines-prompt-empty-"));
  try {
    assert.equal(buildRoutinesPromptSection(dir), "");
    assert.equal(buildSystemPromptAddendum("table", dir), CODE_MODE_ADDENDUM);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("saved routines are listed in the addendum: name -- description (params: ...), most-recent first", () => {
  const dir = makeRepo([
    { name: "older-one", description: "the first thing solved", params: { entity: "X" }, mtime: 1_000_000 },
    { name: "newer-one", description: "the latest thing solved", params: { file: "a", depth: 2 }, mtime: 2_000_000 },
  ]);
  try {
    const section = buildRoutinesPromptSection(dir);
    assert.match(section, /prefer sem\.routine\(name, params\)/);
    // Neither routine is trusted (no session, no .sem/routines.trust, no
    // PI_SEM_ROUTINES_TRUST=all), so both carry the unvetted marker --
    // routine-trust-boundary DESIGN point 3.
    const newerAt = section.indexOf("- newer-one -- the latest thing solved (unvetted: read-only replay) (params: file, depth)");
    const olderAt = section.indexOf("- older-one -- the first thing solved (unvetted: read-only replay) (params: entity)");
    assert.ok(newerAt !== -1 && olderAt !== -1, `both lines must appear, got:\n${section}`);
    assert.ok(newerAt < olderAt, "most-recent routine must be listed first");
    const addendum = buildSystemPromptAddendum("table", dir);
    assert.ok(addendum.startsWith(CODE_MODE_ADDENDUM), "the static addendum stays intact ahead of the listing");
    assert.ok(addendum.includes(section), "the listing must ride the system-prompt addendum");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the listing caps at 10 lines and says how many more sem.routines() would show", () => {
  const dir = makeRepo(Array.from({ length: 12 }, (_, i) => ({ name: `routine-${String(i).padStart(2, "0")}`, mtime: 1_000_000 + i })));
  try {
    const section = buildRoutinesPromptSection(dir);
    assert.equal(section.split("\n").filter((l) => l.startsWith("- ")).length, 10);
    assert.match(section, /\(\+2 more -- sem\.routines\(\) lists all\)/);
    assert.match(section, /- routine-11/, "the newest survives the cap");
    assert.doesNotMatch(section, /- routine-01\b/, "the oldest two fall off");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a routine with an unreadable header still lists, flagged, instead of breaking the prompt build", () => {
  const dir = makeRepo([{ name: "hand-edited", raw: "// not a sem:routine header" }]);
  try {
    assert.match(buildRoutinesPromptSection(dir), /- hand-edited -- \(unreadable header/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
