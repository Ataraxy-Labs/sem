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

  it('parses TSX files with JSX syntax', () => {
    const content = `
      export const App = () => {
        return <main><h1>Hello</h1></main>;
      };
    `;

    const entities = parser.extractEntities(content, 'app.tsx');
    const appEntity = entities.find(e => e.name === 'App');

    expect(appEntity).toBeDefined();
    expect(appEntity?.entityType).toBe('function');
  });

  it('parses JSX files with JSX syntax', () => {
    const content = `
      export const App = () => {
        return <main><h1>Hello</h1></main>;
      };
    `;

    const entities = parser.extractEntities(content, 'app.jsx');
    const appEntity = entities.find(e => e.name === 'App');

    expect(appEntity).toBeDefined();
    expect(appEntity?.entityType).toBe('function');
  });

  it('classifies bindings by declaration kind and assigned function shape in JS and TS', () => {
    const content = `
      const constantValue = "abc";
      let mutableValue = "def";
      var legacyValue = "ghi";

      var arrowFn = () => 1;
      const functionExpr = function () { return 2; };
      let generatorExpr = function* () { yield 3; };
    `;

    for (const filePath of ['bindings.ts', 'bindings.js']) {
      const entities = parser.extractEntities(content, filePath);
      const types = Object.fromEntries(entities.map(entity => [entity.name, entity.entityType]));

      expect(types.constantValue).toBe('constant');
      expect(types.mutableValue).toBe('variable');
      expect(types.legacyValue).toBe('variable');
      expect(types.arrowFn).toBe('function');
      expect(types.functionExpr).toBe('function');
      expect(types.generatorExpr).toBe('generator');
    }
  });

  it('extracts function-like object pairs as methods and skips inner variables', () => {
    const content = `
      export const createHandlers = () => {
        const helper = 1;

        return {
          onClick: () => helper,
          title: 'plain value',
        };
      };
    `;

    const entities = parser.extractEntities(content, 'handlers.ts');
    const names = entities.map(e => e.name);

    expect(names).toContain('createHandlers');
    expect(names).toContain('onClick');
    expect(names).not.toContain('helper');
    expect(names).not.toContain('title');

    const onClick = entities.find(e => e.name === 'onClick');
    expect(onClick?.entityType).toBe('method');
  });
});
