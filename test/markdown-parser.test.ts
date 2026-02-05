import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { MarkdownParserPlugin } from '../src/parser/plugins/markdown/index.js';
import { matchEntities, defaultSimilarity } from '../src/model/identity.js';

const fixtures = resolve(__dirname, 'fixtures');

describe('MarkdownParserPlugin', () => {
  const parser = new MarkdownParserPlugin();

  it('extracts heading-based sections', () => {
    const content = readFileSync(resolve(fixtures, 'before.md'), 'utf-8');
    const entities = parser.extractEntities(content, 'README.md');

    expect(entities.length).toBeGreaterThan(0);
    const names = entities.map(e => e.name);
    expect(names).toContain('Project');
    expect(names).toContain('Getting Started');
    expect(names).toContain('API');
  });

  it('detects heading changes', () => {
    const before = readFileSync(resolve(fixtures, 'before.md'), 'utf-8');
    const after = readFileSync(resolve(fixtures, 'after.md'), 'utf-8');

    const beforeEntities = parser.extractEntities(before, 'README.md');
    const afterEntities = parser.extractEntities(after, 'README.md');

    const result = matchEntities(beforeEntities, afterEntities, 'README.md', defaultSimilarity);

    const changes = Object.fromEntries(result.changes.map(c => [c.entityName, c.changeType]));

    // Project heading modified (new content)
    expect(changes['Project']).toBe('modified');
    // Configuration added
    expect(changes['Configuration']).toBe('added');
    // API modified
    expect(changes['API']).toBe('modified');
  });
});
