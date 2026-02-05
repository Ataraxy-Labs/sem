import type { SimpleGit } from 'simple-git';
import type { FileChange, DiffScope } from './types.js';

export async function readChangedFiles(git: SimpleGit, scope: DiffScope): Promise<FileChange[]> {
  const files: FileChange[] = [];

  switch (scope.type) {
    case 'working': {
      const diff = await git.diff(['--name-status']);
      const untrackedRaw = await git.raw(['ls-files', '--others', '--exclude-standard']);
      files.push(...parseDiffNameStatus(diff));
      // Add untracked files as 'added'
      for (const line of untrackedRaw.split('\n').filter(Boolean)) {
        files.push({ filePath: line.trim(), status: 'added' });
      }
      break;
    }
    case 'staged': {
      const diff = await git.diff(['--cached', '--name-status']);
      files.push(...parseDiffNameStatus(diff));
      break;
    }
    case 'commit': {
      const diff = await git.diff([`${scope.sha}~1`, scope.sha, '--name-status']);
      files.push(...parseDiffNameStatus(diff));
      break;
    }
    case 'range': {
      const diff = await git.diff([scope.from, scope.to, '--name-status']);
      files.push(...parseDiffNameStatus(diff));
      break;
    }
  }

  return files.filter(f => !shouldIgnore(f.filePath));
}

const IGNORED_PREFIXES = ['.sem/', '.sem\\'];

function shouldIgnore(filePath: string): boolean {
  return IGNORED_PREFIXES.some(p => filePath.startsWith(p));
}

function parseDiffNameStatus(output: string): FileChange[] {
  const files: FileChange[] = [];
  for (const line of output.split('\n').filter(Boolean)) {
    const parts = line.split('\t');
    const statusCode = parts[0].trim();

    if (statusCode === 'A') {
      files.push({ filePath: parts[1], status: 'added' });
    } else if (statusCode === 'D') {
      files.push({ filePath: parts[1], status: 'deleted' });
    } else if (statusCode === 'M') {
      files.push({ filePath: parts[1], status: 'modified' });
    } else if (statusCode.startsWith('R')) {
      files.push({ filePath: parts[2], status: 'renamed', oldFilePath: parts[1] });
    }
  }
  return files;
}

export async function getFileContent(git: SimpleGit, filePath: string, ref?: string): Promise<string | undefined> {
  try {
    if (ref) {
      return await git.show([`${ref}:${filePath}`]);
    }
    // Current working copy
    const { readFile } = await import('node:fs/promises');
    const { resolve } = await import('node:path');
    const root = await git.revparse(['--show-toplevel']);
    return await readFile(resolve(root.trim(), filePath), 'utf-8');
  } catch {
    return undefined;
  }
}
