import { describe, it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { YamlParserPlugin } from '../src/parser/plugins/yaml/index.js';
import { matchEntities, defaultSimilarity } from '../src/model/identity.js';

const fixtures = resolve(__dirname, 'fixtures');

describe('YamlParserPlugin', () => {
  const parser = new YamlParserPlugin();

  it('extracts sections and properties from YAML', () => {
    const content = readFileSync(resolve(fixtures, 'before.yml'), 'utf-8');
    const entities = parser.extractEntities(content, 'config.yml');

    expect(entities.length).toBeGreaterThan(0);
    const names = entities.map(e => e.name);
    expect(names).toContain('server');
    expect(names).toContain('server.host');
    expect(names).toContain('server.port');
    expect(names).toContain('database');
  });

  it('detects property changes', () => {
    const before = readFileSync(resolve(fixtures, 'before.yml'), 'utf-8');
    const after = readFileSync(resolve(fixtures, 'after.yml'), 'utf-8');

    const beforeEntities = parser.extractEntities(before, 'config.yml');
    const afterEntities = parser.extractEntities(after, 'config.yml');

    const result = matchEntities(beforeEntities, afterEntities, 'config.yml', defaultSimilarity);

    const changeNames = result.changes.map(c => c.entityName);
    const changeTypes = result.changes.map(c => c.changeType);

    // server.host changed (localhost → 0.0.0.0)
    expect(changeNames).toContain('server.host');
    // database.pool_size added
    expect(changeNames).toContain('database.pool_size');
    expect(changeTypes).toContain('added');
    expect(changeTypes).toContain('modified');
  });
});
