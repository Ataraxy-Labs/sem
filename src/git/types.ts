export type DiffScope =
  | { type: 'working' }       // Unstaged changes
  | { type: 'staged' }        // Staged changes
  | { type: 'commit'; sha: string }  // Single commit
  | { type: 'range'; from: string; to: string };  // Commit range

export interface FileChange {
  filePath: string;
  status: 'added' | 'modified' | 'deleted' | 'renamed';
  oldFilePath?: string;
  beforeContent?: string;
  afterContent?: string;
}

export interface CommitInfo {
  sha: string;
  shortSha: string;
  author: string;
  date: string;
  message: string;
}
