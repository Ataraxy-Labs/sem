import { describe, it, expect, afterEach } from 'vitest';
import { SemDatabase } from '../src/storage/database.js';
import { contentHash } from '../src/utils/hash.js';
import type { SemanticEntity } from '../src/model/entity.js';
import type { SemanticChange } from '../src/model/change.js';

describe('SemDatabase', () => {
  let db: SemDatabase;

  afterEach(() => {
    db?.close();
  });

  it('creates and queries entities', () => {
    db = new SemDatabase(':memory:');

    const entities: SemanticEntity[] = [
      {
        id: 'test.ts::function::greet',
        filePath: 'test.ts',
        entityType: 'function',
        name: 'greet',
        content: 'function greet() {}',
        contentHash: contentHash('function greet() {}'),
        startLine: 1,
        endLine: 3,
      },
    ];

    db.insertEntities(entities, 'current', 'abc123');
    const result = db.getEntities('current');
    expect(result).toHaveLength(1);
    expect(result[0].name).toBe('greet');
  });

  it('stores and queries changes', () => {
    db = new SemDatabase(':memory:');

    const changes: SemanticChange[] = [
      {
        id: 'change::1',
        entityId: 'test.ts::function::greet',
        changeType: 'modified',
        entityType: 'function',
        entityName: 'greet',
        filePath: 'test.ts',
        beforeContent: 'old',
        afterContent: 'new',
        commitSha: 'abc123',
      },
    ];

    db.insertChanges(changes);
    const result = db.getChanges({ filePath: 'test.ts' });
    expect(result).toHaveLength(1);
    expect(result[0].changeType).toBe('modified');
  });

  it('filters changes by type', () => {
    db = new SemDatabase(':memory:');

    const changes: SemanticChange[] = [
      { id: 'c1', entityId: 'e1', changeType: 'added', entityType: 'function', entityName: 'a', filePath: 'a.ts' },
      { id: 'c2', entityId: 'e2', changeType: 'deleted', entityType: 'function', entityName: 'b', filePath: 'b.ts' },
      { id: 'c3', entityId: 'e3', changeType: 'modified', entityType: 'class', entityName: 'c', filePath: 'c.ts' },
    ];

    db.insertChanges(changes);

    expect(db.getChanges({ changeType: 'added' })).toHaveLength(1);
    expect(db.getChanges({ entityType: 'class' })).toHaveLength(1);
  });

  it('handles metadata', () => {
    db = new SemDatabase(':memory:');

    db.setMetadata('version', '0.1.0');
    expect(db.getMetadata('version')).toBe('0.1.0');
    expect(db.getMetadata('nonexistent')).toBeUndefined();
  });

  it('supports raw SQL queries', () => {
    db = new SemDatabase(':memory:');

    db.insertChanges([
      { id: 'c1', entityId: 'e1', changeType: 'added', entityType: 'function', entityName: 'test', filePath: 'a.ts' },
    ]);

    const result = db.query('SELECT count(*) as cnt FROM changes');
    expect(result).toHaveLength(1);
    expect((result[0] as any).cnt).toBe(1);
  });
});
