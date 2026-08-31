import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { runCommand } from "../../../src/tools/internal/proc.ts";
import { repoRelativePath } from "../../../src/tools/internal/git.ts";

/**
 * The symlink relPath bug: `git rev-parse --show-toplevel` resolves symlinks
 * but the caller's absPath was used unresolved, so any path reaching a repo
 * through a symlinked prefix produced a repo-escaping relPath
 * ("../../../..."). On macOS os.tmpdir() sits under /var/folders — itself a
 * symlink to /private/var/folders — making this the ORDINARY case there: two
 * coordinators claiming the same file under the two path spellings keyed it
 * DIFFERENTLY and never collided. The repro below constructs an explicit
 * symlink so it fails on any OS, not incidentally on macOS.
 */

async function makeRepo(): Promise<string> {
  const dir = mkdtempSync(join(tmpdir(), "git-relpath-"));
  await runCommand("git", ["init", "-q"], dir);
  return dir;
}

test("repoRelativePath resolves a symlinked prefix to the same key as the real path", async () => {
  const real = await makeRepo();
  const realResolved = realpathSync(real);
  const filePath = join(realResolved, "src", "lib.rs");
  mkdirSync(join(realResolved, "src"));
  writeFileSync(filePath, "pub fn x() {}\n");

  const linkParent = mkdtempSync(join(tmpdir(), "git-relpath-link-"));
  const link = join(linkParent, "via-link");
  symlinkSync(realResolved, link);

  try {
    const viaReal = await repoRelativePath(filePath);
    const viaLink = await repoRelativePath(join(link, "src", "lib.rs"));
    assert.ok(viaReal && viaLink);
    assert.equal(viaReal.relPath, "src/lib.rs");
    assert.equal(viaLink.relPath, "src/lib.rs", "the symlinked spelling must key identically — this is the collision-detection key");
    assert.doesNotMatch(viaLink.relPath, /\.\./, "never a repo-escaping relative path");
    assert.equal(viaReal.root, viaLink.root);
  } finally {
    rmSync(real, { recursive: true, force: true });
    rmSync(linkParent, { recursive: true, force: true });
  }
});

test("repoRelativePath still works for a target file that does not exist yet (only the directory is realpathed)", async () => {
  const real = await makeRepo();
  try {
    const missing = join(realpathSync(real), "new-file.ts");
    const loc = await repoRelativePath(missing);
    assert.ok(loc);
    assert.equal(loc.relPath, "new-file.ts");
  } finally {
    rmSync(real, { recursive: true, force: true });
  }
});

test("repoRelativePath returns undefined outside any git repository", async () => {
  const dir = mkdtempSync(join(tmpdir(), "git-relpath-norepo-"));
  try {
    const loc = await repoRelativePath(join(dir, "x.ts"));
    assert.equal(loc, undefined);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
