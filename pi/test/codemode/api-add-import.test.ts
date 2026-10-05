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

test("parser edits preserve same-line code, comments and UTF-8 byte offsets", async () => {
  const dir = makeDir({ "a.ts": '// café 😀\nimport { parse, keep } from "./old.js"; const important = "é"; // retained\n' });
  try {
    const { sem } = api(dir);
    await sem.addImport("a.ts", 'import { parse } from "./new.js";');
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    assert.ok(content.startsWith('// café 😀\n'));
    assert.ok(content.includes('const important = "é"; // retained'));
    assert.ok(content.includes('import { keep } from "./old.js";'));
    await sem.addImport("a.ts", 'import { keep } from "./other.js";');
    assert.ok(readFileSync(join(dir, "a.ts"), "utf8").includes('const important = "é"; // retained'));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("fixture text is not a duplicate when there are no top-level imports", async () => {
  const original = '"use strict";\nexport const fixture = `\nimport { parse } from "./new.js";\n`;\n';
  const dir = makeDir({ "a.ts": original });
  try {
    const { sem } = api(dir);
    assert.equal((await sem.addImport("a.ts", 'import { parse } from "./new.js";')).added, true);
    assert.ok(readFileSync(join(dir, "a.ts"), "utf8").startsWith(original));
    assert.equal((await sem.addImport("a.ts", 'import { parse } from "./new.js";')).alreadyPresent, true);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("missing parser refuses mutation rather than unsafe fallback", async () => {
  const original = 'import { parse } from "./old.js";\n';
  const dir = makeDir({ "a.ts": original });
  try {
    const sem = buildSemApi({ cwd: dir, semBin: join(dir, "missing-sem"), changes: createChangeLog() });
    await assert.rejects(() => sem.addImport("a.ts", 'import { parse } from "./new.js";'), /parser unavailable/);
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), original);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("invalid syntax refuses mutation", async () => {
  const original = 'import { broken';
  const dir = makeDir({ "a.ts": original });
  try {
    await assert.rejects(() => api(dir).sem.addImport("a.ts", 'import { x } from "./x.js";'), /parser unavailable/);
    assert.equal(readFileSync(join(dir, "a.ts"), "utf8"), original);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("multiple same-line imports and CRLF preserve all non-import source", async () => {
  const dir = makeDir({ "a.ts": 'import { x } from "./x.js"; import { y } from "./y.js"; const keep = 1;\r\n' });
  try {
    const { sem } = api(dir);
    await sem.addImport("a.ts", 'import { x, y } from "./new.js";');
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    assert.ok(content.includes('const keep = 1;\r\n'));
    assert.ok(content.includes('import { x, y } from "./new.js";\r\n'));
    assert.ok(!content.includes('from "./x.js"'));
    assert.ok(!content.includes('from "./y.js"'));
    assert.equal((await sem.addImport("a.ts", 'import { x, y } from "./new.js";')).alreadyPresent, true);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("Go grouped and standalone imports are idempotent with either spec syntax", async () => {
  for (const declaration of ['import (\n\talias "example.com/lib"\n)', 'import alias "example.com/lib"']) {
    const original = `package shared\n\n${declaration}\n\nfunc f() {}\n`;
    const dir = makeDir({ "a.go": original });
    try {
      const { sem } = api(dir);
      for (const spec of ['alias "example.com/lib"', 'import alias "example.com/lib"']) {
        assert.equal((await sem.addImport("a.go", spec)).alreadyPresent, true);
        assert.equal(readFileSync(join(dir, "a.go"), "utf8"), original);
      }
    } finally { rmSync(dir, { recursive: true, force: true }); }
  }
});

test("Java imports follow the package and any existing imports", async () => {
  for (const existing of ["", "\nimport java.util.List;\n"]) {
    const dir = makeDir({ "A.java": `// License\npackage example.app;\n${existing}\nclass A {}\n` });
    try {
      const { sem } = api(dir);
      await sem.addImport("A.java", "import java.util.Map;");
      const content = readFileSync(join(dir, "A.java"), "utf8");
      assert.ok(content.indexOf("package example.app;") < content.indexOf("import java.util.Map;"));
      if (existing) assert.ok(content.indexOf("import java.util.List;") < content.indexOf("import java.util.Map;"));
      assert.ok(content.indexOf("import java.util.Map;") < content.indexOf("class A"));
      assert.equal((await sem.addImport("A.java", "import java.util.Map;")).alreadyPresent, true);
    } finally { rmSync(dir, { recursive: true, force: true }); }
  }
});

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

test("adds a Python import after __future__ imports", async () => {
  const dir = makeDir({
    "a.py": '# Copyright\n\n"""Module docs."""\n\nfrom __future__ import annotations\n\nimport os\n\nVALUE = os.name\n',
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.py", "from .env_utils import is_env_enabled")) as AddImportResult;
    assert.equal(r.added, true);
    assert.equal(r.line, 8);
    assert.match(
      readFileSync(join(dir, "a.py"), "utf8"),
      /"""Module docs\."""\n\nfrom __future__ import annotations\n\nimport os\nfrom \.env_utils import is_env_enabled\n/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("adds a Go import inside the package import block", async () => {
  const dir = makeDir({
    "a.go": 'package shared\n\nimport (\n\t"fmt"\n)\n\nfunc f() { fmt.Println() }\n',
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.go", 'internalgit "example.com/internal/git"')) as AddImportResult;
    assert.equal(r.added, true);
    assert.equal(r.line, 5);
    assert.match(
      readFileSync(join(dir, "a.go"), "utf8"),
      /import \(\n\t"fmt"\n\tinternalgit "example\.com\/internal\/git"\n\)/,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("adds a first Go import after the package declaration", async () => {
  const dir = makeDir({
    "a.go": "package shared\n\nfunc f() {}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.go", '"example.com/internal/git"')) as AddImportResult;
    assert.equal(r.added, true);
    assert.equal(r.line, 3);
    assert.equal(
      readFileSync(join(dir, "a.go"), "utf8"),
      'package shared\n\nimport "example.com/internal/git"\nfunc f() {}\n',
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("adds a C include after the leading include block", async () => {
  const dir = makeDir({
    "a.c": '/** file docs */\n#include "base.h"\n#include <stdint.h>\n\nint main(void) { return 0; }\n',
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.c", '#include "compat.h"')) as AddImportResult;
    assert.equal(r.added, true);
    assert.equal(r.line, 4);
    assert.equal(
      readFileSync(join(dir, "a.c"), "utf8"),
      '/** file docs */\n#include "base.h"\n#include <stdint.h>\n#include "compat.h"\n\nint main(void) { return 0; }\n',
    );
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

test("supersede stays inside the leading import block, not in a fixture string", async () => {
  const original = [
    'import { parse } from "./parser.js";',
    "",
    "export const FIXTURE = `",
    'import { parse } from "./legacy.js";',
    "export const value = parse();",
    "`;",
    "",
  ].join("\n");
  const dir = makeDir({ "a.ts": original });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { parse } from "./parser-v2.js";')) as AddImportResult;
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    // The real import is superseded...
    assert.deepEqual(r.superseded, [{ symbol: "parse", from: "./parser.js" }]);
    assert.doesNotMatch(content, /"\.\/parser\.js"/);
    // ...and the fixture's own source text is left exactly as it was.
    assert.match(content, /export const FIXTURE = `\nimport \{ parse \} from "\.\/legacy\.js";\nexport const value = parse\(\);\n`;/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("supersede ignores an indented import inside an ambient module block", async () => {
  const original = [
    'import { parse } from "./parser.js";',
    "",
    'declare module "legacy" {',
    '  import { parse } from "./legacy.js";',
    "  export const value: typeof parse;",
    "}",
    "",
  ].join("\n");
  const dir = makeDir({ "a.d.ts": original });
  try {
    const { sem } = api(dir);
    await sem.addImport("a.d.ts", 'import { parse } from "./parser-v2.js";');
    const content = readFileSync(join(dir, "a.d.ts"), "utf8");
    assert.match(content, /^  import \{ parse \} from "\.\/legacy\.js";$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an import-shaped line inside a block comment is never superseded", async () => {
  const original = [
    "/*",
    'import { parse } from "./inside-comment.js";',
    "*/",
    'import { parse } from "./real.js";',
    "export const v = parse;",
    "",
  ].join("\n");
  const dir = makeDir({ "a.ts": original });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { parse } from "./new.js";')) as AddImportResult;
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    // Only the real import is reported and rewritten; the commented one is not.
    assert.deepEqual(r.superseded, [{ symbol: "parse", from: "./real.js" }]);
    assert.doesNotMatch(content, /"\.\/real\.js"/);
    assert.match(content, /import \{ parse \} from "\.\/new\.js";/);
    // The comment's text is left untouched.
    assert.match(content, /\/\*\nimport \{ parse \} from "\.\/inside-comment\.js";\n\*\//);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a new import lands after a multi-line import, not inside its braces", async () => {
  const original = [
    "import {",
    "  a,",
    "  b",
    '} from "./ab.js";',
    "",
    "export const v = a;",
    "",
  ].join("\n");
  const dir = makeDir({ "a.ts": original });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { c } from "./c.js";')) as AddImportResult;
    assert.equal(r.added, true);
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    assert.match(content, /\} from "\.\/ab\.js";\nimport \{ c \} from "\.\/c\.js";\n/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("supersede rewrites a multi-line import, collapsing it to one line", async () => {
  const original = [
    "import {",
    "  parse,",
    "  stringify",
    '} from "./old.js";',
    "export const v = parse;",
    "",
  ].join("\n");
  const dir = makeDir({ "a.ts": original });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { parse } from "./new.js";')) as AddImportResult;
    assert.deepEqual(r.superseded, [{ symbol: "parse", from: "./old.js" }]);
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    assert.match(content, /^import \{ stringify \} from "\.\/old\.js";$/m);
    assert.match(content, /^import \{ parse \} from "\.\/new\.js";$/m);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a new import follows an import placed after other statements", async () => {
  const original = [
    "const x = 1;",
    'import { y } from "./y.js";',
    "export const z = y;",
    "",
  ].join("\n");
  const dir = makeDir({ "a.ts": original });
  try {
    const { sem } = api(dir);
    const r = (await sem.addImport("a.ts", 'import { w } from "./w.js";')) as AddImportResult;
    assert.equal(r.added, true);
    const content = readFileSync(join(dir, "a.ts"), "utf8");
    // The new import follows the existing import, even though that import
    // itself follows a statement -- it is not hoisted above `const x`.
    assert.match(content, /import \{ y \} from "\.\/y\.js";\nimport \{ w \} from "\.\/w\.js";/);
    assert.match(content, /^const x = 1;/);
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
