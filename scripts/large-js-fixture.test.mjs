import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {
  generateLargeJsFixture,
  parseImportStyleMix,
  parseLargeJsFixtureArgs,
} from './large-js-fixture.mjs';

async function withTempDirectory(callback) {
  const tempDirectory = await fs.mkdtemp(path.join(os.tmpdir(), 'sem-large-js-fixture-test-'));

  try {
    return await callback(tempDirectory);
  } finally {
    await fs.rm(tempDirectory, { recursive: true, force: true });
  }
}

test('generates a deterministic TypeScript fixture with every import style', async () => {
  await withTempDirectory(async (tempDirectory) => {
    const outDir = path.join(tempDirectory, 'fixture');
    const options = {
      outDir,
      fileCount: 5,
      entitiesPerFile: 2,
      importFanout: 4,
      importStyleMix: parseImportStyleMix('named,default,namespace,type'),
      nestedDepth: 2,
      bodyLines: 3,
      language: 'ts',
      gitInit: false,
    };

    const first = await generateLargeJsFixture(options);
    const firstSource = await fs.readFile(path.join(outDir, 'src', 'f0000.ts'), 'utf8');
    const firstManifest = await fs.readFile(path.join(outDir, 'fixture-manifest.json'), 'utf8');

    const second = await generateLargeJsFixture(options);
    const secondSource = await fs.readFile(path.join(outDir, 'src', 'f0000.ts'), 'utf8');
    const secondManifest = await fs.readFile(path.join(outDir, 'fixture-manifest.json'), 'utf8');

    assert.equal(first.manifest.files.length, 5);
    assert.equal(second.manifest.files.length, 5);
    assert.equal(firstSource, secondSource);
    assert.equal(firstManifest, secondManifest);

    assert.match(firstSource, /import \{ f0001_e000 as imported_f0001_0 \}/);
    assert.match(firstSource, /import default_f0002_1 from '\.\/f0002';/);
    assert.match(firstSource, /import \* as ns_f0003_2 from '\.\/f0003';/);
    assert.match(firstSource, /import type \{ T0004_000 as Type_f0004_3 \}/);
    assert.match(firstSource, /function f0000_e000\(input: number\): number/);
    assert.match(firstSource, /function f0000_e000_nested_1\(value: number\): number/);
    assert.match(firstSource, /const incrementalMarker_f0000_e000: number = 0;/);
  });
});

test('mixed fixtures avoid TypeScript-only syntax in JavaScript files', async () => {
  await withTempDirectory(async (tempDirectory) => {
    const outDir = path.join(tempDirectory, 'fixture');

    await generateLargeJsFixture({
      outDir,
      fileCount: 2,
      entitiesPerFile: 1,
      importFanout: 1,
      importStyleMix: parseImportStyleMix('type'),
      nestedDepth: 1,
      bodyLines: 1,
      language: 'mixed',
      gitInit: false,
    });

    const tsSource = await fs.readFile(path.join(outDir, 'src', 'f0000.ts'), 'utf8');
    const jsSource = await fs.readFile(path.join(outDir, 'src', 'f0001.js'), 'utf8');

    assert.doesNotMatch(tsSource, /import type/);
    assert.match(tsSource, /import \{ f0001_e000 as imported_f0001_0 \}/);
    assert.doesNotMatch(jsSource, /: number/);
    assert.doesNotMatch(jsSource, /import type/);
    assert.match(jsSource, /function f0001_e000\(input\)/);
  });
});

test('parses CLI knobs and weighted import style mixes', () => {
  const parsed = parseLargeJsFixtureArgs([
    '--out',
    'tmp-fixture',
    '--files',
    '7',
    '--entities-per-file',
    '3',
    '--fanout',
    '2',
    '--import-style-mix',
    'named:2,default',
    '--nested-depth',
    '1',
    '--body-lines',
    '5',
    '--language',
    'js',
    '--no-git-init',
    '--force',
  ]);

  assert.equal(parsed.outDir, path.resolve('tmp-fixture'));
  assert.equal(parsed.fileCount, 7);
  assert.equal(parsed.entitiesPerFile, 3);
  assert.equal(parsed.importFanout, 2);
  assert.deepEqual(parsed.importStyleMix, ['named', 'named', 'default']);
  assert.equal(parsed.nestedDepth, 1);
  assert.equal(parsed.bodyLines, 5);
  assert.equal(parsed.language, 'js');
  assert.equal(parsed.gitInit, false);
  assert.equal(parsed.force, true);

  assert.throws(() => parseImportStyleMix('named:0'), /positive integer/);
  assert.throws(() => parseLargeJsFixtureArgs(['--files', '10abc']), /positive integer/);
});

test('protects non-fixture directories unless force is set', async () => {
  await withTempDirectory(async (tempDirectory) => {
    const outDir = path.join(tempDirectory, 'occupied');
    await fs.mkdir(outDir, { recursive: true });
    await fs.writeFile(path.join(outDir, 'keep.txt'), 'do not replace\n', 'utf8');

    await assert.rejects(
      () =>
        generateLargeJsFixture({
          outDir,
          fileCount: 1,
          entitiesPerFile: 1,
          importFanout: 0,
          gitInit: false,
        }),
      /Refusing to replace non-empty directory/,
    );

    await generateLargeJsFixture({
      outDir,
      fileCount: 1,
      entitiesPerFile: 1,
      importFanout: 0,
      gitInit: false,
      force: true,
    });

    await assert.rejects(() => fs.stat(path.join(outDir, 'keep.txt')), /ENOENT/);
  });
});
