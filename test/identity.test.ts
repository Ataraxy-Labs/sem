import { describe, it, expect } from 'vitest';
import { matchEntities, defaultSimilarity } from '../src/model/identity.js';
import { contentHash } from '../src/utils/hash.js';
import type { SemanticEntity } from '../src/model/entity.js';

function makeEntity(overrides: Partial<SemanticEntity> & { id: string; name: string; content: string }): SemanticEntity {
  return {
    filePath: 'test.ts',
    entityType: 'function',
    startLine: 1,
    endLine: 10,
    contentHash: contentHash(overrides.content),
    ...overrides,
  };
}

describe('matchEntities', () => {
  it('detects exact ID match as modified', () => {
    const before = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content: 'function greet() { return "hi"; }' })];
    const after = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content: 'function greet() { return "hello"; }' })];

    const result = matchEntities(before, after, 'test.ts');
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('modified');
  });

  it('detects unchanged entity (no change produced)', () => {
    const content = 'function greet() { return "hi"; }';
    const before = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content })];
    const after = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content })];

    const result = matchEntities(before, after, 'test.ts');
    expect(result.changes).toHaveLength(0);
  });

  it('detects added entities', () => {
    const before: SemanticEntity[] = [];
    const after = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content: 'function greet() {}' })];

    const result = matchEntities(before, after, 'test.ts');
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('added');
  });

  it('detects deleted entities', () => {
    const before = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content: 'function greet() {}' })];
    const after: SemanticEntity[] = [];

    const result = matchEntities(before, after, 'test.ts');
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('deleted');
  });

  it('detects renamed via content hash', () => {
    const content = 'function greet() { return "hi"; }';
    const before = [makeEntity({ id: 'test.ts::fn::greet', name: 'greet', content })];
    const after = [makeEntity({ id: 'test.ts::fn::sayHello', name: 'sayHello', content })];

    const result = matchEntities(before, after, 'test.ts');
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('renamed');
  });

  it('detects moved via content hash with different file path', () => {
    const content = 'function greet() { return "hi"; }';
    const before = [makeEntity({ id: 'old.ts::fn::greet', name: 'greet', content, filePath: 'old.ts' })];
    const after = [makeEntity({ id: 'new.ts::fn::greet', name: 'greet', content, filePath: 'new.ts' })];

    const result = matchEntities(before, after, 'test.ts');
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('moved');
  });

  it('uses fuzzy similarity for renamed with modified content', () => {
    const before = [makeEntity({
      id: 'test.ts::fn::calculateTotal',
      name: 'calculateTotal',
      content: 'function calculateTotal(items) { return items.reduce((sum, item) => sum + item.price, 0); }',
    })];
    const after = [makeEntity({
      id: 'test.ts::fn::computeTotal',
      name: 'computeTotal',
      content: 'function computeTotal(items) { return items.reduce((sum, item) => sum + item.price, 0); }',
    })];

    const result = matchEntities(before, after, 'test.ts', defaultSimilarity);
    expect(result.changes).toHaveLength(1);
    expect(result.changes[0].changeType).toBe('renamed');
  });
});

describe('defaultSimilarity', () => {
  it('returns 1 for identical content', () => {
    const a = makeEntity({ id: 'a', name: 'a', content: 'hello world' });
    const b = makeEntity({ id: 'b', name: 'b', content: 'hello world' });
    expect(defaultSimilarity(a, b)).toBe(1);
  });

  it('returns 0 for completely different content', () => {
    const a = makeEntity({ id: 'a', name: 'a', content: 'alpha beta gamma' });
    const b = makeEntity({ id: 'b', name: 'b', content: 'delta epsilon zeta' });
    expect(defaultSimilarity(a, b)).toBe(0);
  });

  it('returns partial score for overlapping content', () => {
    const a = makeEntity({ id: 'a', name: 'a', content: 'hello world foo bar' });
    const b = makeEntity({ id: 'b', name: 'b', content: 'hello world baz qux' });
    const score = defaultSimilarity(a, b);
    expect(score).toBeGreaterThan(0);
    expect(score).toBeLessThan(1);
  });
});
