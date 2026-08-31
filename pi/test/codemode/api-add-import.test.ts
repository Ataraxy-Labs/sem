import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSemApi, createChangeLog } from "../../src/codemode/api.ts";
import type { AddImportResult } from "../../src/codemode/api.ts";

/**
 * sem.addImport(): the creation-adjacent gap sem.edit() can't fill —
 * import/mod lines aren't entities. Raw-file discipline (write()'s, not
 * weave-coordinated: an import line is not an entity, and a synthetic
 * whole-file claim would be smuggled semantics). Idempotent, and for ES
 * named imports it supersedes a stale import of the same symbol(s) from a
 * different source.
 */

function makeDir(files: Record<string, string>): string {
  const dir = mkdtempSync(join(tmpdir(), "add-import-"));
  for (const [name, content] of Object.entries(files)) writeFileSync(join(dir, name), content);
  return dir;
}

const api = (dir: string, changes = createChangeLog()) => ({ sem: buildSemApi({ cwd: dir, semBin: "sem", changes }), changes });

test("adds a Rust mod declaration after existing mods", async () => {
  const dir = makeDir({ "lib.rs": "pub mod alpha;\nmod beta;\n\npub fn x() {}\n" });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("lib.rs", "pub mod gamma;")) as AddImportResult;
    assert.equal(r.added, true);
    assert.equal(r.line, 3);
    assert.match(readFileSync(join(dir, "lib.rs"), "utf8"), /mod beta;\npub mod gamma;\n/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("adds an ES named import after existing imports", async () => {
  const dir = makeDir({ "a.ts": 'import { one } from "./one.js";\n\nexport const v = one;\n' });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { two } from "./two.js"')) as AddImportResult;
    assert.equal(r.added, true);
    assert.match(readFileSync(join(dir, "a.ts"), "utf8"), /one\.js";\nimport \{ two \} from "\.\/two\.js";\n/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("idempotent: the same spec again (whitespace/semicolon normalized) reports alreadyPresent", async () => {
  const dir = makeDir({ "a.ts": 'import { one } from "./one.js";\n' });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", '  import   { one }   from "./one.js"  ')) as AddImportResult;
    assert.equal(r.added, false);
    assert.equal(r.alreadyPresent, true);
    assert.equal(r.line, 1);
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), 'import { one } from "./one.js";\n', "no duplicate written");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a Rust mod dedupes across visibility variants of the same module", async () => {
  const dir = makeDir({ "lib.rs": "pub mod gamma;\n" });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("lib.rs", "mod gamma;")) as AddImportResult;
    assert.equal(r.added, false);
    assert.equal(r.alreadyPresent, true);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("supersede with overlap: the moved symbol leaves the stale import, the rest stays", async () => {
  const dir = makeDir({ "a.ts": 'import { moved, stays } from "./old.js";\n' });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { moved } from "./new.js";')) as AddImportResult;
    assert.equal(r.added, true);
    assert.deepEqual(r.superseded, [{ symbol: "moved", from: "./old.js" }]);
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    assert.match(content, /import \{ stays \} from "\.\/old\.js";/);
    assert.match(content, /import \{ moved \} from "\.\/new\.js";/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("supersede full drop: an import left empty is removed, not left as import {}", async () => {
  const dir = makeDir({ "a.ts": 'import { moved } from "./old.js";\nexport const v = moved;\n' });
  try {
    const { sem } = api(dir);
    await sem.addImport("a.ts", 'import { moved } from "./new.js";');
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    assert.doesNotMatch(content, /old\.js/);
    assert.match(content, /import \{ moved \} from "\.\/new\.js";/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an unrelated import from another source is untouched by supersede", async () => {
  const dir = makeDir({ "a.ts": 'import { other } from "./other.js";\n' });
  try {
    const { sem } = api(dir);
    await sem.addImport("a.ts", 'import { moved } from "./new.js";');
    assert.match(readFileSync(join(dir, "a.ts"), "utf8"), /import \{ other \} from "\.\/other\.js";/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a missing file throws an actionable error instead of creating it", async () => {
  const dir = makeDir({});
  try {
    const { sem } = api(dir);
    await assert.rejects(sem.addImport("nope.ts", 'import { x } from "./x.js";'), /sem\.addImport: file "nope\.ts" not found/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a function-local use line does not attract the insert into the function body", async () => {
  const dir = makeDir({
    "lib.rs": [
      "use std::collections::HashMap;",
      "",
      "pub fn hash_it() -> u64 {",
      "    use std::collections::hash_map::DefaultHasher;",
      "    0",
      "}",
      "",
    ].join("\n"),
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("lib.rs", "use crate::similarity::token_jaccard;")) as AddImportResult;
    assert.equal(r.line, 2, "must insert after the top-level import block, not after the function-local use");
    const lines = readFileSync(join(dir, "lib.rs"), "utf8").split("\n");
    assert.equal(lines[1], "use crate::similarity::token_jaccard;");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an indented lookalike declaration does not count as already present", async () => {
  const dir = makeDir({
    "lib.rs": "pub fn f() {\n    use std::fmt::Write;\n}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("lib.rs", "use std::fmt::Write;")) as AddImportResult;
    assert.equal(r.added, true, "the function-scoped use is a different scope, not this file-level declaration");
    assert.equal(r.line, 1);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a multi-line use group is consumed whole -- the insert cannot land inside its braces", async () => {
  const dir = makeDir({
    "lib.rs": ["use crate::conflict::{", "    classify_conflict, MergeStats,", "};", "", "pub fn f() {}", ""].join("\n"),
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("lib.rs", "use crate::similarity::token_jaccard;")) as AddImportResult;
    assert.equal(r.line, 4, "must insert after the closing `};` of the multi-line use group");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("unindented import-shaped lines inside string literals below the import block are ignored", async () => {
  const dir = makeDir({
    "fixtures.rs": [
      "use std::fmt::Write;",
      "",
      "pub const FIXTURE: &str = r#\"",
      'import { a } from "./somewhere.js"',
      "use fake::embedded;",
      "\"#;",
      "",
    ].join("\n"),
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("fixtures.rs", "use crate::real::thing;")) as AddImportResult;
    assert.equal(r.line, 2, "the leading import block ends at line 1; embedded source text is not an import");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("changed() integration: an addImport is recorded in the session ChangeLog", async () => {
  const dir = makeDir({ "a.ts": 'import { one } from "./one.js";\n' });
  try {
    const { sem, changes } = api(dir);
    await sem.addImport("a.ts", 'import { two } from "./two.js";');
    const entries = changes.list();
    assert.equal(entries.length, 1);
    assert.equal(entries[0]!.file, "a.ts");
    assert.equal(entries[0]!.op, "addImport");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
