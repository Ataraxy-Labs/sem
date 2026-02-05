import simpleGit, { type SimpleGit } from 'simple-git';
import type { DiffScope, FileChange, CommitInfo } from './types.js';
import { readChangedFiles, parseDiffNameStatus, getFileContent } from './diff-reader.js';

export class GitBridge {
  private git: SimpleGit;
  private repoRoot: string;

  constructor(repoPath: string) {
    this.repoRoot = repoPath;
    this.git = simpleGit(repoPath);
  }

  async isRepo(): Promise<boolean> {
    try {
      await this.git.revparse(['--is-inside-work-tree']);
      return true;
    } catch {
      return false;
    }
  }

  async getRepoRoot(): Promise<string> {
    const root = await this.git.revparse(['--show-toplevel']);
    return root.trim();
  }

  async getChangedFiles(scope: DiffScope): Promise<FileChange[]> {
    const files = await readChangedFiles(this.git, scope);

    // Fetch all file contents in parallel
    await Promise.all(files.map(file => this.populateContent(file, scope)));

    return files;
  }

  /**
   * Combined detect + get files in one shot.
   * Eliminates the redundant git calls between detectScope() and getChangedFiles().
   * Uses --name-status directly (detects scope from results, skips the --name-only round).
   */
  async detectAndGetFiles(): Promise<{ scope: DiffScope; files: FileChange[] }> {
    // Single parallel batch: isRepo check + staged + working + untracked
    const [, stagedRaw, workingRaw, untrackedRaw] = await Promise.all([
      this.git.revparse(['--is-inside-work-tree']),      // validates repo
      this.git.diff(['--cached', '--name-status']),       // staged changes with status
      this.git.diff(['--name-status']),                   // working changes with status
      this.git.raw(['ls-files', '--others', '--exclude-standard']),  // untracked
    ]);

    let scope: DiffScope;
    let files: FileChange[];

    if (stagedRaw.trim()) {
      scope = { type: 'staged' };
      files = parseDiffNameStatus(stagedRaw).filter(f => !f.filePath.startsWith('.sem/'));
    } else if (workingRaw.trim() || untrackedRaw.trim()) {
      scope = { type: 'working' };
      files = parseDiffNameStatus(workingRaw);
      for (const line of untrackedRaw.split('\n').filter(Boolean)) {
        files.push({ filePath: line.trim(), status: 'added' });
      }
      files = files.filter(f => !f.filePath.startsWith('.sem/'));
    } else {
      try {
        const head = await this.getHeadSha();
        scope = { type: 'commit', sha: head };
        files = await readChangedFiles(this.git, scope);
      } catch {
        return { scope: { type: 'working' }, files: [] };
      }
    }

    // Fetch all file contents in parallel
    await Promise.all(files.map(file => this.populateContent(file, scope)));

    return { scope, files };
  }

  private async populateContent(file: FileChange, scope: DiffScope): Promise<void> {
    const fetches: Promise<void>[] = [];

    switch (scope.type) {
      case 'working': {
        if (file.status !== 'deleted') {
          fetches.push(getFileContent(this.git, file.filePath, undefined, this.repoRoot).then(c => { file.afterContent = c; }));
        }
        if (file.status !== 'added') {
          fetches.push(getFileContent(this.git, file.filePath, 'HEAD').then(c => { file.beforeContent = c; }));
        }
        break;
      }
      case 'staged': {
        if (file.status !== 'deleted') {
          fetches.push(
            this.git.show([`:${file.filePath}`])
              .then(c => { file.afterContent = c; })
              .catch(() => getFileContent(this.git, file.filePath, undefined, this.repoRoot).then(c => { file.afterContent = c; }))
          );
        }
        if (file.status !== 'added') {
          fetches.push(getFileContent(this.git, file.filePath, 'HEAD').then(c => { file.beforeContent = c; }));
        }
        break;
      }
      case 'commit': {
        if (file.status !== 'deleted') {
          fetches.push(getFileContent(this.git, file.filePath, scope.sha).then(c => { file.afterContent = c; }));
        }
        if (file.status !== 'added') {
          fetches.push(getFileContent(this.git, file.filePath, `${scope.sha}~1`).then(c => { file.beforeContent = c; }));
        }
        break;
      }
      case 'range': {
        if (file.status !== 'deleted') {
          fetches.push(getFileContent(this.git, file.filePath, scope.to).then(c => { file.afterContent = c; }));
        }
        if (file.status !== 'added') {
          fetches.push(getFileContent(this.git, file.oldFilePath ?? file.filePath, scope.from).then(c => { file.beforeContent = c; }));
        }
        break;
      }
    }

    await Promise.all(fetches);
  }

  /**
   * Detect scope + check repo in a single batch.
   * Reduces 3 sequential git calls to parallel.
   */
  async detectScope(): Promise<DiffScope> {
    const [staged, working, untracked] = await Promise.all([
      this.git.diff(['--cached', '--name-only']),
      this.git.diff(['--name-only']),
      this.git.raw(['ls-files', '--others', '--exclude-standard']),
    ]);

    if (staged.trim()) {
      return { type: 'staged' };
    }

    if (working.trim() || untracked.trim()) {
      return { type: 'working' };
    }

    try {
      const head = await this.getHeadSha();
      return { type: 'commit', sha: head };
    } catch {
      return { type: 'working' };
    }
  }

  async getLog(limit: number = 20): Promise<CommitInfo[]> {
    const log = await this.git.log({ maxCount: limit });
    return log.all.map(entry => ({
      sha: entry.hash,
      shortSha: entry.hash.slice(0, 7),
      author: entry.author_name,
      date: entry.date,
      message: entry.message,
    }));
  }

  async getCurrentBranch(): Promise<string> {
    const branch = await this.git.revparse(['--abbrev-ref', 'HEAD']);
    return branch.trim();
  }

  async getHeadSha(): Promise<string> {
    const sha = await this.git.revparse(['HEAD']);
    return sha.trim();
  }
}
