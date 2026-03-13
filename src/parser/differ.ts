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

    const fallback = registry.getPluginById('fallback');

    if (file.beforeContent) {
      try {
        beforeEntities = plugin.extractEntities(file.beforeContent, file.oldFilePath ?? file.filePath);
      } catch {
        if (plugin !== fallback && fallback) {
          beforeEntities = fallback.extractEntities(file.beforeContent, file.oldFilePath ?? file.filePath);
        }
      }
    }

    if (file.afterContent) {
      try {
        afterEntities = plugin.extractEntities(file.afterContent, file.filePath);
      } catch {
        if (plugin !== fallback && fallback) {
          afterEntities = fallback.extractEntities(file.afterContent, file.filePath);
        }
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

  // Single-pass counting
  let addedCount = 0, modifiedCount = 0, deletedCount = 0, movedCount = 0, renamedCount = 0;
  for (const c of allChanges) {
    switch (c.changeType) {
      case 'added': addedCount++; break;
      case 'modified': modifiedCount++; break;
      case 'deleted': deletedCount++; break;
      case 'moved': movedCount++; break;
      case 'renamed': renamedCount++; break;
    }
  }

  return {
    changes: allChanges,
    fileCount: filesWithChanges.size,
    addedCount,
    modifiedCount,
    deletedCount,
    movedCount,
    renamedCount,
  };
}
