import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { JsonParserPlugin } from '../src/parser/plugins/json/index.js';
import { matchEntities, defaultSimilarity } from '../src/model/identity.js';

const fixtures = resolve(__dirname, 'fixtures');

describe('JsonParserPlugin', () => {
  const parser = new JsonParserPlugin();

  it('extracts properties from JSON', () => {
    const content = readFileSync(resolve(fixtures, 'before.json'), 'utf-8');
    const entities = parser.extractEntities(content, 'config.json');

    expect(entities.length).toBeGreaterThan(0);
    const names = entities.map(e => e.name);
    expect(names).toContain('name');
    expect(names).toContain('version');
    expect(names).toContain('settings');
  });

  it('detects changes between before and after', () => {
    const before = readFileSync(resolve(fixtures, 'before.json'), 'utf-8');
    const after = readFileSync(resolve(fixtures, 'after.json'), 'utf-8');

    const beforeEntities = parser.extractEntities(before, 'config.json');
    const afterEntities = parser.extractEntities(after, 'config.json');

    const result = matchEntities(beforeEntities, afterEntities, 'config.json', defaultSimilarity);

    const changeTypes = result.changes.map(c => c.changeType);
    const changedNames = result.changes.map(c => c.entityName);

    // version changed 1.0.0 → 2.0.0
    expect(changeTypes).toContain('modified');
    // logLevel added
    expect(changeTypes).toContain('added');
    expect(changedNames).toContain('logLevel');
  });

  it('handles empty JSON object', () => {
    const entities = parser.extractEntities('{}', 'empty.json');
    expect(entities).toEqual([]);
  });

  it('handles nested objects', () => {
    const content = JSON.stringify({ a: { b: { c: 1 } } });
    const entities = parser.extractEntities(content, 'nested.json');
    const names = entities.map(e => e.name);
    expect(names).toContain('a');
    expect(names).toContain('b');
    expect(names).toContain('c');
  });
});
