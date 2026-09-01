import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { extractEntities, resolveEntity, type Entity } from "../../src/tools/internal/entities.ts";
import { checkDependents } from "../../src/tools/internal/impact.ts";
import { performSemRead } from "../../src/tools/sem-read.ts";
import { performWeaveEdit } from "../../src/tools/weave-edit.ts";
import { buildSemApi } from "../../src/codemode/api.ts";

/**
 * P4 of the 2026-09-02 transcript study, all three halves.
 *
 * (a) SILENT WRONG ENTITY. django__django-13128: after `entity_type:"method"`
 *     was refused, the agent fell back to
 *     `{name:"resolve_expression", entity_type:"function", ordinal:1}`.
 *     `ordinal` indexes a start-line-sorted candidate list, and index 1 was
 *     `ResolvedOuterRef.resolve_expression` -- an unrelated method that
 *     raises NotSupportedError -- whose body was overwritten with
 *     `CombinedExpression` logic. This is the ONLY case in the 327-run
 *     corpus where the tool did the wrong thing without erroring. A bare
 *     ordinal among same-named entities that `parent_name` would separate
 *     must be refused with the candidate list, not silently indexed.
 *
 * (b) SELF-CONTRADICTORY NOT-FOUND. `resolveEntity` filters by name, then
 *     entity_type, then parent_name; on an empty result it reports
 *     `not-found` with `nearestNames` computed over the UNFILTERED entity
 *     list -- so when the FILTER emptied a non-empty name match, the model
 *     is told the name does not exist and then shown that same name as the
 *     closest match. 43 times across 39 distinct runs (12% of the corpus).
 *
 * (c) FALSE AMBIGUITY IN edit().impact. The post-edit dependents check
 *     re-resolved by BARE NAME, discarding the disambiguators that had just
 *     resolved the edit, so `sem impact` refused it. 33 receipts across 30
 *     runs read `"impact": "not checked (error: Entity name 'deconstruct'
 *     is ambiguous (5 matches)...)"` on an edit that landed correctly, and
 *     django__django-10914 read that as "the edit didn't land" and retried.
 */

function entity(over: Partial<Entity> & { name: string }): Entity {
  return {
    type: "function",
    start_line: 1,
    end_line: 2,
    parent_id: null,
    parentName: null,
    byteRangeReliable: true,
    ...over,
  };
}

const TWO_CLASSES = [
  "class ResolvedOuterRef:",
  "    def resolve_expression(self):",
  "        raise NotSupportedError('not supported here')",
  "",
  "class CombinedExpression:",
  "    def resolve_expression(self):",
  "        return self.lhs",
  "",
  "def caller():",
  "    return CombinedExpression().resolve_expression()",
  "",
].join("\n");

/** A committed git repo, so sem's reference graph indexes the file the same way it does on a real checkout. */
function withTempDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
  const dir = mkdtempSync(join(tmpdir(), "entity-resolution-honesty-"));
  writeFileSync(join(dir, "m.py"), TWO_CLASSES, "utf8");
  for (const args of [["init", "-q"], ["config", "user.email", "test@example.com"], ["config", "user.name", "Test"], ["add", "m.py"], ["commit", "-q", "-m", "initial"]]) {
    execFileSync("git", args, { cwd: dir, stdio: "ignore" });
  }
  return run(dir).finally(() => rmSync(dir, { recursive: true, force: true }));
}

// --- (a) ordinal must be earned -------------------------------------------

test("P4a: a bare ordinal is REFUSED when parent_name would disambiguate, instead of silently picking", () => {
  const first = entity({ name: "resolve_expression", start_line: 2, parent_id: "m.py::class::ResolvedOuterRef", parentName: "ResolvedOuterRef" });
  const second = entity({ name: "resolve_expression", start_line: 6, parent_id: "m.py::class::CombinedExpression", parentName: "CombinedExpression" });

  const result = resolveEntity([first, second], { name: "resolve_expression", ordinal: 1 });

  assert.equal(result.kind, "ambiguous", "ordinal must not silently select among parent-distinguishable entities");
  assert.equal(result.kind === "ambiguous" && result.candidates.length, 2);
  assert.equal(result.kind === "ambiguous" && result.ordinalRefused, true);
});

test("P4a: ordinal still resolves when parent_name genuinely CANNOT disambiguate (two overloads, same parent)", () => {
  const first = entity({ name: "overload", start_line: 1 });
  const second = entity({ name: "overload", start_line: 9 });

  const result = resolveEntity([first, second], { name: "overload", ordinal: 1 });

  assert.equal(result.kind, "found");
  assert.equal(result.kind === "found" && result.entity.start_line, 9);
});

test("P4a: sem_read's refusal names parent_name as the fix rather than accepting the ordinal", async () => {
  await withTempDir(async (dir) => {
    const outcome = await performSemRead(
      { entity: { name: "resolve_expression", ordinal: 1 }, file: "m.py" },
      { cwd: dir, semBin: "sem" },
    );
    assert.equal(outcome.isError, true, "an ordinal among parent-distinguishable entities must be refused");
    assert.match(outcome.text, /parent_name/);
    assert.match(outcome.text, /ResolvedOuterRef/);
    assert.match(outcome.text, /CombinedExpression/);
  });
});

test("P4a: weave_edit refuses the ordinal rather than overwriting the wrong same-named method", async () => {
  await withTempDir(async (dir) => {
    const outcome = await performWeaveEdit(
      {
        file: "m.py",
        entity: { name: "resolve_expression", ordinal: 1 },
        op: "replace",
        content: "    def resolve_expression(self):\n        return 'overwritten'",
      },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, true, "the django-13128 silent wrong-entity edit must not be reachable");
    assert.match(outcome.text, /parent_name/);
  });
});

// --- (b) a filter-emptied match must say so -------------------------------

test("P4b: a type filter that empties a NON-empty name match says so, never 'no entity named X ... closest: X'", () => {
  const asFunction = entity({ name: "in_bulk", type: "function", parentName: "QuerySet" });

  const result = resolveEntity([asFunction], { name: "in_bulk", entity_type: "method" });

  assert.equal(result.kind, "not-found");
  assert.ok(result.kind === "not-found" && result.filteredOut !== undefined, "the name-only matches must be carried through");
  assert.equal(result.kind === "not-found" && result.filteredOut?.length, 1);
});

test("P4b: sem_read reports 'N entities named X exist but none match {filters}' with the real candidates", async () => {
  await withTempDir(async (dir) => {
    const outcome = await performSemRead(
      { entity: { name: "resolve_expression", entity_type: "method" }, file: "m.py" },
      { cwd: dir, semBin: "sem" },
    );
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /2 entities named "resolve_expression" exist/);
    assert.match(outcome.text, /none match/);
    assert.match(outcome.text, /entity_type:"method"/);
    assert.doesNotMatch(outcome.text, /Closest names/, "the self-contradictory 'closest: X' line must be gone");
  });
});

test("P4b: weave_edit reports the same filter-emptied truth instead of a bare not-found", async () => {
  await withTempDir(async (dir) => {
    const outcome = await performWeaveEdit(
      { file: "m.py", entity: { name: "caller", entity_type: "method" }, op: "replace", content: "def caller():\n    return 1" },
      { cwd: dir, semBin: "sem", coordinator: undefined },
    );
    assert.equal(outcome.isError, true);
    assert.match(outcome.text, /1 entity named "caller" exists/);
    assert.match(outcome.text, /entity_type:"method"/);
  });
});

// --- (c) the post-edit impact check must not re-resolve by bare name ------

test("P4c: checkDependents answers by resolved entity id where the bare name is refused as ambiguous", async () => {
  await withTempDir(async (dir) => {
    const abs = join(dir, "m.py");
    const entities = await extractEntities("sem", abs, dir);
    const resolved = resolveEntity(entities, { name: "resolve_expression", parent_name: "ResolvedOuterRef" });
    assert.equal(resolved.kind, "found");
    const target = resolved.kind === "found" ? resolved.entity : undefined;
    assert.ok(target);

    const byName = await checkDependents("sem", dir, abs, target.name);
    assert.equal(byName.ok, false, "the bare-name lookup is what produced the false ambiguity");
    assert.match(byName.ok === false ? byName.reason : "", /ambiguous/);

    const byId = await checkDependents("sem", dir, abs, target.name, undefined, "m.py::class::ResolvedOuterRef::resolve_expression");
    assert.equal(byId.ok, true, "the id-addressed lookup answers the question the caller already resolved");
    assert.deepEqual(byId.ok === true ? byId.dependents.map((d) => d.name) : [], ["caller"]);
  });
});

test("P4c: edit().impact carries real callers for a parent-disambiguated entity, never false ambiguity", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const result = (await api.edit({
      file: "m.py",
      entity: { name: "resolve_expression", parent_name: "ResolvedOuterRef" },
      op: "replace",
      content: "    def resolve_expression(self):\n        raise NotSupportedError('still not supported')",
    })) as { impact: string };

    assert.doesNotMatch(result.impact, /ambiguous/, `impact re-resolved by bare name: ${result.impact}`);
    assert.doesNotMatch(result.impact, /not checked/, `impact was abandoned: ${result.impact}`);
    assert.equal(result.impact, "1 caller (caller)");
  });
});

test("P4c: the same edit on the OTHER same-named method reports honestly rather than erroring out", async () => {
  await withTempDir(async (dir) => {
    const api = buildSemApi({ cwd: dir, semBin: "sem" });

    const result = (await api.edit({
      file: "m.py",
      entity: { name: "resolve_expression", parent_name: "CombinedExpression" },
      op: "replace",
      content: "    def resolve_expression(self):\n        return self.rhs",
    })) as { impact: string };

    assert.doesNotMatch(result.impact, /ambiguous/, `impact re-resolved by bare name: ${result.impact}`);
    assert.equal(result.impact, "no direct callers");
  });
});
