export type ChangeType = 'added' | 'modified' | 'deleted' | 'moved' | 'renamed';

export interface SemanticChange {
  id: string;
  entityId: string;
  changeType: ChangeType;
  entityType: string;
  entityName: string;
  filePath: string;
  oldFilePath?: string;
  beforeContent?: string;
  afterContent?: string;
  commitSha?: string;
  author?: string;
  timestamp?: string;
}
