import { basename, dirname, join, relative, sep } from "node:path";
import { realpathSync } from "node:fs";
import { runCommand } from "./proc.ts";

/**
 * weave-mcp's coordination tools address files as repo-root-relative POSIX
 * paths. These helpers are best-effort: any failure (not a git repo, no
 * `git` on PATH) resolves to `undefined` / a placeholder branch rather than
 * throwing, since coordination is optional and must never block the disk
 * edit.
 */

export interface RepoLocation {
  root: string;
  relPath: string;
}

export async function repoRelativePath(absPath: string): Promise<RepoLocation | undefined> {
  /*
   * `git rev-parse --show-toplevel` resolves symlinks; computing
   * relative(root, absPath) against the caller's UNRESOLVED path therefore
   * produced a bogus, repo-escaping relPath ("../../../var/folders/.../x.ts")
   * for any path reaching the repo through a symlinked prefix — which is the
   * ORDINARY case on macOS, where os.tmpdir() lives under /var/folders, a
   * symlink to /private/var/folders. Two coordinators claiming "the same"
   * file under different absolute-path spellings then registered under two
   * different relPath keys and never collided at all — silently disabling
   * the exact collision detection this module exists for. Realpath the
   * CONTAINING DIRECTORY (not the full path: the target file may not exist
   * on disk yet) before computing the relative offset.
   */
  const dir = dirname(absPath);
  let realDir: string;
  try {
    realDir = realpathSync(dir);
  } catch {
    return undefined;
  }
  const result = await runCommand("git", ["rev-parse", "--show-toplevel"], realDir);
  if (result.exitCode !== 0) return undefined;

  const root = result.stdout.trim();
  if (!root) return undefined;

  const relPath = relative(root, join(realDir, basename(absPath))).split(sep).join("/");
  return { root, relPath };
}

export async function currentBranch(root: string): Promise<string> {
  const result = await runCommand("git", ["rev-parse", "--abbrev-ref", "HEAD"], root);
  const branch = result.exitCode === 0 ? result.stdout.trim() : "";
  return branch || "HEAD";
}
