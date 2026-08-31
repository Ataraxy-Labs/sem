import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { buildSemApi, createChangeLog } from "../../src/codemode/api.ts";
import type { AddResult } from "../../src/codemode/api.ts";

/**
 * sem.add(): the creation door -- a NEW file plus its mod/export wiring in
 * one call, module->file resolution from the project layout, and
 * REFUSE-NOT-GUESS everywhere resolution is ambiguous. In pure mode it is
 * the ONLY way to create a file (sem.write refuses there -- pinned in
 * test/bridge/extension-pure-mode.test.ts).
 */

function makeTree(files: Record<string, string>): string {
  const dir = mkdtempSync(join(tmpdir(), "sem-add-"));
  for (const [name, content] of Object.entries(files)) {
    mkdirSync(join(dir, name, ".."), { recursive: true });
    writeFileSync(join(dir, name), content);
  }
  return dir;
}

const api = (dir: string, changes = createChangeLog()) => ({ sem: buildSemApi({ cwd: dir, semBin: "sem", changes }), changes });

const CHECKSUM_RS = "pub fn fnv1a(data: &[u8]) -> u64 {\n    let mut h: u64 = 0xcbf29ce484222325;\n    for b in data {\n        h ^= *b as u64;\n        h = h.wrapping_mul(0x100000001b3);\n    }\n    h\n}\n";

test("module add in a single crate: creates src/<module>.rs and wires `mod X;` into lib.rs", async () => {
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "fixture"\n',
    "src/lib.rs": "pub mod existing;\n\npub fn top() {}\n",
    "src/existing.rs": "pub fn e() {}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({ module: "checksum", content: CHECKSUM_RS })) as AddResult;
    assert.equal(r.file, "src/checksum.rs");
    assert.equal(r.created, true);
    assert.equal(readFileSync(join(dir, "src/checksum.rs"), "utf8"), CHECKSUM_RS);
    assert.ok(r.wired, "a module add must wire the declaration");
    assert.equal(r.wired.file, "src/lib.rs");
    assert.equal(r.wired.spec, "pub mod checksum;", "content's top-level pub fn implies an exported mod");
    assert.match(readFileSync(join(dir, "src/lib.rs"), "utf8"), /^pub mod existing;\npub mod checksum;\n/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("module add wires into main.rs when the crate has no lib.rs", async () => {
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "bin-fixture"\n',
    "src/main.rs": "fn main() {}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({ module: "checksum", content: CHECKSUM_RS })) as AddResult;
    assert.equal(r.wired?.file, "src/main.rs");
    assert.match(readFileSync(join(dir, "src/main.rs"), "utf8"), /^pub mod checksum;\nfn main/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("ambiguous module resolution (workspace with several crates) refuses and names the candidates", async () => {
  const dir = makeTree({
    "crates/a/Cargo.toml": '[package]\nname = "a"\n',
    "crates/a/src/lib.rs": "pub fn a() {}\n",
    "crates/b/Cargo.toml": '[package]\nname = "b"\n',
    "crates/b/src/lib.rs": "pub fn b() {}\n",
  });
  try {
    const { sem } = api(dir);
    await assert.rejects(sem.add({ module: "checksum", content: CHECKSUM_RS }), (err: Error) => {
      assert.match(err.message, /ambiguous/);
      assert.match(err.message, /crates\/a/);
      assert.match(err.message, /crates\/b/);
      assert.match(err.message, /Pass spec\.file/);
      return true;
    });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("TS barrel: module add creates src/<module>.ts and wires an export line into src/index.ts", async () => {
  const dir = makeTree({
    "src/index.ts": 'export * from "./existing.js";\n',
    "src/existing.ts": "export const e = 1;\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({ module: "checksum", content: "export const fnv1a = (n: number): number => n;\n" })) as AddResult;
    assert.equal(r.file, "src/checksum.ts");
    assert.equal(r.wired?.file, "src/index.ts");
    assert.match(readFileSync(join(dir, "src/index.ts"), "utf8"), /export \* from "\.\/checksum\.js";/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("file add of a non-module path writes exactly there and wires nothing", async () => {
  const dir = makeTree({ "README.md": "x\n" });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({ file: "notes/design.md", content: "# design\n" })) as AddResult;
    assert.equal(r.file, "notes/design.md");
    assert.equal(r.wired, undefined);
    assert.equal(readFileSync(join(dir, "notes/design.md"), "utf8"), "# design\n");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("file add of a crate module path still wires -- the workspace escape hatch is not a wiring downgrade", async () => {
  const dir = makeTree({
    "crates/a/Cargo.toml": '[package]\nname = "a"\n',
    "crates/a/src/lib.rs": "pub fn a() {}\n",
    "crates/b/Cargo.toml": '[package]\nname = "b"\n',
    "crates/b/src/lib.rs": "pub fn b() {}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({ file: "crates/b/src/checksum.rs", content: CHECKSUM_RS })) as AddResult;
    assert.equal(r.wired?.file, "crates/b/src/lib.rs");
    assert.equal(r.wired?.spec, "pub mod checksum;");
    assert.match(readFileSync(join(dir, "crates/b/src/lib.rs"), "utf8"), /^pub mod checksum;\n/);
    assert.ok(!readFileSync(join(dir, "crates/a/src/lib.rs"), "utf8").includes("checksum"), "the OTHER crate is untouched");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("an existing target refuses toward sem.edit/sem.addImport instead of overwriting", async () => {
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "fixture"\n',
    "src/lib.rs": "mod checksum;\n",
    "src/checksum.rs": "pub fn old() {}\n",
  });
  try {
    const { sem } = api(dir);
    await assert.rejects(sem.add({ module: "checksum", content: CHECKSUM_RS }), /already exists.*sem\.edit|sem\.edit.*already exists/s);
    assert.equal(readFileSync(join(dir, "src/checksum.rs"), "utf8"), "pub fn old() {}\n", "nothing overwritten");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("unresolvable module (no crate, no barrel) refuses with the file: escape hatch", async () => {
  const dir = makeTree({ "notes.txt": "n\n" });
  try {
    const { sem } = api(dir);
    await assert.rejects(sem.add({ module: "checksum", content: "x" }), /cannot resolve module.*Pass spec\.file/s);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("exactly one of file/module is required, and content is required", async () => {
  const dir = makeTree({});
  try {
    const { sem } = api(dir);
    await assert.rejects(sem.add({ content: "x" } as never), /exactly one of spec\.file.*spec\.module/s);
    await assert.rejects(sem.add({ file: "a.rs", module: "a", content: "x" } as never), /exactly one/);
    await assert.rejects(sem.add({ module: "a" } as never), /content/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("changed() records the add (and its wiring) in the session ChangeLog", async () => {
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "fixture"\n',
    "src/lib.rs": "pub fn top() {}\n",
  });
  try {
    const { sem, changes } = api(dir);
    await sem.add({ module: "checksum", content: CHECKSUM_RS });
    const ops = changes.list().map((e) => `${e.op}:${e.file}`);
    assert.ok(ops.includes("add:src/checksum.rs"), `expected add op, got ${ops}`);
    assert.ok(ops.includes("addImport:src/lib.rs"), `expected wiring op, got ${ops}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pub-ness inference: content with a top-level pub item wires `pub mod X;`, not a private mod", async () => {
  // The rust-create-01 probe shape: an external integration test importing
  // weave_core::checksum::fnv1a compiles only if the MODULE is public --
  // a plain `mod checksum;` around a `pub fn` is unreachable from outside
  // the crate, and the probe RED-ed on exactly that.
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "fixture"\n',
    "src/lib.rs": "pub fn top() {}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({ module: "checksum", content: CHECKSUM_RS })) as AddResult;
    assert.equal(r.wired?.spec, "pub mod checksum;");
    assert.match(readFileSync(join(dir, "src/lib.rs"), "utf8"), /^pub mod checksum;\n/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("pub-ness inference: pub(crate)-only or private content keeps the mod private", async () => {
  const dir = makeTree({
    "Cargo.toml": '[package]\nname = "fixture"\n',
    "src/lib.rs": "pub fn top() {}\n",
  });
  try {
    const { sem } = api(dir);
    const r = (await sem.add({
      module: "internal_helpers",
      content: "pub(crate) fn helper() {}\n\nfn private_helper() {}\n",
    })) as AddResult;
    assert.equal(r.wired?.spec, "mod internal_helpers;", "pub(crate) is crate-internal -- the mod must not be exported");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
