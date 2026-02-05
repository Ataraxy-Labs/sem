import simpleGit, { type SimpleGit } from 'simple-git';
import type { DiffScope, FileChange, CommitInfo } from './types.js';
import { readChangedFiles, getFileContent } from './diff-reader.js';

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

    // Populate file contents
    for (const file of files) {
      switch (scope.type) {
        case 'working': {
          if (file.status !== 'deleted') {
            file.afterContent = await getFileContent(this.git, file.filePath);
          }
          if (file.status !== 'added') {
            file.beforeContent = await getFileContent(this.git, file.filePath, 'HEAD');
          }
          break;
        }
        case 'staged': {
          if (file.status !== 'deleted') {
            // Staged content via git show :filePath
            try {
              file.afterContent = await this.git.show([`:${file.filePath}`]);
            } catch {
              file.afterContent = await getFileContent(this.git, file.filePath);
            }
          }
          if (file.status !== 'added') {
            file.beforeContent = await getFileContent(this.git, file.filePath, 'HEAD');
          }
          break;
        }
        case 'commit': {
          if (file.status !== 'deleted') {
            file.afterContent = await getFileContent(this.git, file.filePath, scope.sha);
          }
          if (file.status !== 'added') {
            file.beforeContent = await getFileContent(this.git, file.filePath, `${scope.sha}~1`);
          }
          break;
        }
        case 'range': {
          if (file.status !== 'deleted') {
            file.afterContent = await getFileContent(this.git, file.filePath, scope.to);
          }
          if (file.status !== 'added') {
            file.beforeContent = await getFileContent(this.git, file.oldFilePath ?? file.filePath, scope.from);
          }
          break;
        }
      }
    }

    return files;
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

  /**
   * Detect the default scope for `sem diff`:
   * - If there are staged changes, use 'staged'
   * - If there are unstaged changes, use 'working'
   * - Otherwise fall back to HEAD~1..HEAD
   */
  async detectScope(): Promise<DiffScope> {
    const staged = await this.git.diff(['--cached', '--name-only']);
    if (staged.trim()) {
      return { type: 'staged' };
    }

    const working = await this.git.diff(['--name-only']);
    const untrackedRaw = await this.git.raw(['ls-files', '--others', '--exclude-standard']);
    if (working.trim() || untrackedRaw.trim()) {
      return { type: 'working' };
    }

    // No local changes — show last commit
    try {
      const head = await this.getHeadSha();
      return { type: 'commit', sha: head };
    } catch {
      return { type: 'working' };
    }
  }
}
