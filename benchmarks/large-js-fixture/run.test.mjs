import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { buildSemCommand, parseLargeJsFixtureBenchmarkArgs } from './run.mjs';

test('normalizes path-like sem executables before fixture cwd changes', () => {
  const parsed = parseLargeJsFixtureBenchmarkArgs([
    '--sem',
    'crates/target/debug/sem',
    '--out',
    '/tmp/sem-large-js-repo',
    '--cache-dir',
    '/tmp/sem-large-js-cache',
  ]);

  assert.equal(parsed.sem, path.resolve('crates/target/debug/sem'));
});

test('keeps plain sem executable names as PATH lookups', () => {
  const parsed = parseLargeJsFixtureBenchmarkArgs([
    '--sem',
    'sem',
    '--out',
    '/tmp/sem-large-js-repo',
    '--cache-dir',
    '/tmp/sem-large-js-cache',
  ]);

  assert.equal(parsed.sem, 'sem');
});

test('rejects cache directories inside the fixture root', () => {
  assert.throws(
    () =>
      parseLargeJsFixtureBenchmarkArgs([
        '--out',
        '/tmp/sem-large-js-repo',
        '--cache-dir',
        '/tmp/sem-large-js-repo/cache',
      ]),
    /--cache-dir must be outside --out/,
  );
});

test('builds graph benchmark command', () => {
  const command = buildSemCommand(
    { command: 'graph' },
    { entryFile: 'src/f0000.ts', targetEntity: 'f0000_e000' },
  );

  assert.deepEqual(command, {
    kind: 'graph',
    args: ['graph', '.', '--json', '--file-exts', '.ts', '.js'],
  });
});

test('defaults impact benchmark command to deps mode', () => {
  const command = buildSemCommand(
    { command: 'impact' },
    { entryFile: 'src/f0000.ts', targetEntity: 'f0000_e000' },
  );

  assert.equal(command.kind, 'impact');
  assert.equal(command.impactMode, 'deps');
  assert.deepEqual(command.args, [
    'impact',
    'f0000_e000',
    '--file',
    'src/f0000.ts',
    '--deps',
    '--json',
    '--file-exts',
    '.ts',
    '.js',
  ]);
});

test('builds requested impact benchmark mode', () => {
  const command = buildSemCommand(
    {
      command: 'impact',
      impactMode: 'dependents',
      targetEntity: 'customEntity',
      targetFile: 'src/custom.ts',
    },
    { entryFile: 'src/f0000.ts', targetEntity: 'f0000_e000' },
  );

  assert.deepEqual(command.args, [
    'impact',
    'customEntity',
    '--file',
    'src/custom.ts',
    '--dependents',
    '--json',
    '--file-exts',
    '.ts',
    '.js',
  ]);
});

test('builds context and verify benchmark commands', () => {
  const manifest = { entryFile: 'src/f0000.ts', targetEntity: 'f0000_e000' };

  assert.deepEqual(buildSemCommand({ command: 'verify' }, manifest), {
    kind: 'verify',
    args: ['verify', '--json', '--file-exts', '.ts', '.js'],
  });
  assert.deepEqual(buildSemCommand({ command: 'context', contextBudget: 1234 }, manifest), {
    kind: 'context',
    targetEntity: 'f0000_e000',
    targetFile: 'src/f0000.ts',
    args: [
      'context',
      'f0000_e000',
      '--file',
      'src/f0000.ts',
      '--budget',
      '1234',
      '--json',
      '--file-exts',
      '.ts',
      '.js',
    ],
  });
});
