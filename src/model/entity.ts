export interface SemanticEntity {
  /** Unique ID: "filePath::entityType::name" or "filePath::parentId::name" */
  id: string;
  filePath: string;
  entityType: string;
  name: string;
  parentId?: string;
  content: string;
  contentHash: string;
  startLine: number;
  endLine: number;
  metadata?: Record<string, string>;
}

export function buildEntityId(filePath: string, entityType: string, name: string, parentId?: string): string {
  if (parentId) {
    return `${filePath}::${parentId}::${name}`;
  }
  return `${filePath}::${entityType}::${name}`;
}
