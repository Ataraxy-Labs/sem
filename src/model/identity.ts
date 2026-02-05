import type { SemanticEntity } from './entity.js';
import type { SemanticChange, ChangeType } from './change.js';
import { contentHash } from '../utils/hash.js';

export interface MatchResult {
  changes: SemanticChange[];
}

/**
 * 3-phase entity matching algorithm:
 * 1. Exact ID match — same entity ID in before/after → modified or unchanged
 * 2. Content hash match — same hash, different ID → renamed or moved
 * 3. Fuzzy similarity — >80% content similarity → probable rename
 */
export function matchEntities(
  before: SemanticEntity[],
  after: SemanticEntity[],
  filePath: string,
  similarityFn?: (a: SemanticEntity, b: SemanticEntity) => number,
  commitSha?: string,
  author?: string,
): MatchResult {
  const changes: SemanticChange[] = [];
  const matchedBefore = new Set<string>();
  const matchedAfter = new Set<string>();

  const beforeById = new Map(before.map(e => [e.id, e]));
  const afterById = new Map(after.map(e => [e.id, e]));

  // Phase 1: Exact ID match
  for (const [id, afterEntity] of afterById) {
    const beforeEntity = beforeById.get(id);
    if (beforeEntity) {
      matchedBefore.add(id);
      matchedAfter.add(id);
      if (beforeEntity.contentHash !== afterEntity.contentHash) {
        changes.push({
          id: `change::${id}`,
          entityId: id,
          changeType: 'modified',
          entityType: afterEntity.entityType,
          entityName: afterEntity.name,
          filePath: afterEntity.filePath,
          beforeContent: beforeEntity.content,
          afterContent: afterEntity.content,
          commitSha,
          author,
        });
      }
      // Unchanged entities produce no change record
    }
  }

  // Collect unmatched
  const unmatchedBefore = before.filter(e => !matchedBefore.has(e.id));
  const unmatchedAfter = after.filter(e => !matchedAfter.has(e.id));

  // Phase 2: Content hash match (rename/move detection)
  const beforeByHash = new Map<string, SemanticEntity[]>();
  for (const entity of unmatchedBefore) {
    const existing = beforeByHash.get(entity.contentHash) ?? [];
    existing.push(entity);
    beforeByHash.set(entity.contentHash, existing);
  }

  for (const afterEntity of [...unmatchedAfter]) {
    const candidates = beforeByHash.get(afterEntity.contentHash);
    if (candidates && candidates.length > 0) {
      const beforeEntity = candidates.shift()!;
      if (candidates.length === 0) {
        beforeByHash.delete(afterEntity.contentHash);
      }
      matchedBefore.add(beforeEntity.id);
      matchedAfter.add(afterEntity.id);

      const changeType: ChangeType =
        beforeEntity.filePath !== afterEntity.filePath ? 'moved' : 'renamed';

      changes.push({
        id: `change::${afterEntity.id}`,
        entityId: afterEntity.id,
        changeType,
        entityType: afterEntity.entityType,
        entityName: afterEntity.name,
        filePath: afterEntity.filePath,
        oldFilePath: beforeEntity.filePath !== afterEntity.filePath ? beforeEntity.filePath : undefined,
        beforeContent: beforeEntity.content,
        afterContent: afterEntity.content,
        commitSha,
        author,
      });
    }
  }

  // Phase 3: Fuzzy similarity (>80% threshold)
  const stillUnmatchedBefore = unmatchedBefore.filter(e => !matchedBefore.has(e.id));
  const stillUnmatchedAfter = unmatchedAfter.filter(e => !matchedAfter.has(e.id));

  if (similarityFn && stillUnmatchedBefore.length > 0 && stillUnmatchedAfter.length > 0) {
    const THRESHOLD = 0.8;

    for (const afterEntity of stillUnmatchedAfter) {
      let bestMatch: SemanticEntity | null = null;
      let bestScore = 0;

      for (const beforeEntity of stillUnmatchedBefore) {
        if (matchedBefore.has(beforeEntity.id)) continue;
        if (beforeEntity.entityType !== afterEntity.entityType) continue;

        const score = similarityFn(beforeEntity, afterEntity);
        if (score > bestScore && score >= THRESHOLD) {
          bestScore = score;
          bestMatch = beforeEntity;
        }
      }

      if (bestMatch) {
        matchedBefore.add(bestMatch.id);
        matchedAfter.add(afterEntity.id);

        const changeType: ChangeType =
          bestMatch.filePath !== afterEntity.filePath ? 'moved' : 'renamed';

        changes.push({
          id: `change::${afterEntity.id}`,
          entityId: afterEntity.id,
          changeType,
          entityType: afterEntity.entityType,
          entityName: afterEntity.name,
          filePath: afterEntity.filePath,
          oldFilePath: bestMatch.filePath !== afterEntity.filePath ? bestMatch.filePath : undefined,
          beforeContent: bestMatch.content,
          afterContent: afterEntity.content,
          commitSha,
          author,
        });
      }
    }
  }

  // Remaining unmatched before = deleted
  for (const entity of before.filter(e => !matchedBefore.has(e.id))) {
    changes.push({
      id: `change::deleted::${entity.id}`,
      entityId: entity.id,
      changeType: 'deleted',
      entityType: entity.entityType,
      entityName: entity.name,
      filePath: entity.filePath,
      beforeContent: entity.content,
      commitSha,
      author,
    });
  }

  // Remaining unmatched after = added
  for (const entity of after.filter(e => !matchedAfter.has(e.id))) {
    changes.push({
      id: `change::added::${entity.id}`,
      entityId: entity.id,
      changeType: 'added',
      entityType: entity.entityType,
      entityName: entity.name,
      filePath: entity.filePath,
      afterContent: entity.content,
      commitSha,
      author,
    });
  }

  return { changes };
}

/** Default content similarity using Jaccard index on tokens */
export function defaultSimilarity(a: SemanticEntity, b: SemanticEntity): number {
  const tokensA = new Set(a.content.split(/\s+/));
  const tokensB = new Set(b.content.split(/\s+/));
  const intersection = new Set([...tokensA].filter(t => tokensB.has(t)));
  const union = new Set([...tokensA, ...tokensB]);
  if (union.size === 0) return 0;
  return intersection.size / union.size;
}
