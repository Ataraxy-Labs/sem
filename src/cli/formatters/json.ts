import type { DiffResult } from '../../parser/differ.js';

export function formatJson(result: DiffResult): string {
  return JSON.stringify({
    summary: {
      fileCount: result.fileCount,
      added: result.addedCount,
      modified: result.modifiedCount,
      deleted: result.deletedCount,
      moved: result.movedCount,
      renamed: result.renamedCount,
      total: result.changes.length,
    },
    changes: result.changes.map(c => ({
      entityId: c.entityId,
      changeType: c.changeType,
      entityType: c.entityType,
      entityName: c.entityName,
      filePath: c.filePath,
      oldFilePath: c.oldFilePath,
      beforeContent: c.beforeContent,
      afterContent: c.afterContent,
      commitSha: c.commitSha,
      author: c.author,
    })),
  }, null, 2);
}
