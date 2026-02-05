import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { CodeParserPlugin } from '../src/parser/plugins/code/index.js';
import { matchEntities, defaultSimilarity } from '../src/model/identity.js';

const fixtures = resolve(__dirname, 'fixtures');

describe('CodeParserPlugin', () => {
  const parser = new CodeParserPlugin();

  it('extracts functions and classes from TypeScript', () => {
    const content = readFileSync(resolve(fixtures, 'before.ts'), 'utf-8');
    const entities = parser.extractEntities(content, 'test.ts');

    expect(entities.length).toBeGreaterThan(0);
    const names = entities.map(e => e.name);
    expect(names).toContain('greet');
    expect(names).toContain('farewell');
    expect(names).toContain('Calculator');
  });

  it('detects function added/deleted/modified', () => {
    const before = readFileSync(resolve(fixtures, 'before.ts'), 'utf-8');
    const after = readFileSync(resolve(fixtures, 'after.ts'), 'utf-8');

    const beforeEntities = parser.extractEntities(before, 'test.ts');
    const afterEntities = parser.extractEntities(after, 'test.ts');

    const result = matchEntities(beforeEntities, afterEntities, 'test.ts', defaultSimilarity);

    const changes = Object.fromEntries(result.changes.map(c => [c.entityName, c.changeType]));

    // greet modified (new parameter)
    expect(changes['greet']).toBe('modified');
    // farewell deleted
    expect(changes['farewell']).toBe('deleted');
    // welcome added
    expect(changes['welcome']).toBe('added');
    // Calculator modified (new method)
    expect(changes['Calculator']).toBe('modified');
  });

  it('extracts entity types correctly', () => {
    const content = readFileSync(resolve(fixtures, 'before.ts'), 'utf-8');
    const entities = parser.extractEntities(content, 'test.ts');

    const greet = entities.find(e => e.name === 'greet');
    expect(greet?.entityType).toBe('function');

    const calc = entities.find(e => e.name === 'Calculator');
    expect(calc?.entityType).toBe('class');
  });
});
