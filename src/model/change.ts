import type { EntityType } from './entity-type.js';
export type ChangeType = 'added' | 'modified' | 'deleted' | 'moved' | 'renamed';

export interface SemanticChange {
  id: string;
  entityId: string;
  changeType: ChangeType;
  entityType: EntityType;
  entityName: string;
  filePath: string;
  oldFilePath?: string;
  beforeContent?: string;
  afterContent?: string;
  commitSha?: string;
  author?: string;
  timestamp?: string;
}
