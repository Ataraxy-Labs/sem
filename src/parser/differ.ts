import type { SemanticChange } from '../model/change.js';
import type { SemanticEntity } from '../model/entity.js';
import type { SemanticParserPlugin } from './plugin.js';
import type { ParserRegistry } from './registry.js';
import type { FileChange } from '../git/types.js';
import { matchEntities, defaultSimilarity } from '../model/identity.js';

export interface DiffResult {
  changes: SemanticChange[];
  fileCount: number;
  addedCount: number;
  modifiedCount: number;
  deletedCount: number;
  movedCount: number;
  renamedCount: number;
}

export function computeSemanticDiff(
  fileChanges: FileChange[],
  registry: ParserRegistry,
  commitSha?: string,
  author?: string,
): DiffResult {
  const allChanges: SemanticChange[] = [];
  const filesWithChanges = new Set<string>();

  for (const file of fileChanges) {
    const plugin = registry.getPlugin(file.filePath);
    if (!plugin) continue;

    let beforeEntities: SemanticEntity[] = [];
    let afterEntities: SemanticEntity[] = [];

    if (file.beforeContent) {
      try {
        beforeEntities = plugin.extractEntities(file.beforeContent, file.oldFilePath ?? file.filePath);
      } catch {
        // If parsing fails, skip this file's before content
      }
    }

    if (file.afterContent) {
      try {
        afterEntities = plugin.extractEntities(file.afterContent, file.filePath);
      } catch {
        // If parsing fails, skip this file's after content
      }
    }

    // For renamed files, remap before entity IDs to use old file path for matching
    const similarityFn = plugin.computeSimilarity ?? defaultSimilarity;

    const result = matchEntities(
      beforeEntities,
      afterEntities,
      file.filePath,
      similarityFn,
      commitSha,
      author,
    );

    if (result.changes.length > 0) {
      filesWithChanges.add(file.filePath);
      allChanges.push(...result.changes);
    }
  }

  return {
    changes: allChanges,
    fileCount: filesWithChanges.size,
    addedCount: allChanges.filter(c => c.changeType === 'added').length,
    modifiedCount: allChanges.filter(c => c.changeType === 'modified').length,
    deletedCount: allChanges.filter(c => c.changeType === 'deleted').length,
    movedCount: allChanges.filter(c => c.changeType === 'moved').length,
    renamedCount: allChanges.filter(c => c.changeType === 'renamed').length,
  };
}
