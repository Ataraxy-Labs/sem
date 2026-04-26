import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { getReleaseDownloadUrl, resolveReleaseArtifact, syncPackageVersion } from './package-meta.mjs';

test('resolveReleaseArtifact maps supported targets to release assets', () => {
  assert.equal(
    resolveReleaseArtifact({ platform: 'linux', arch: 'x64' }),
    'sem-linux-x86_64.tar.gz',
  );
  assert.equal(
    resolveReleaseArtifact({ platform: 'linux', arch: 'arm64' }),
    'sem-linux-arm64.tar.gz',
  );
  assert.equal(
    resolveReleaseArtifact({ platform: 'darwin', arch: 'arm64' }),
    'sem-darwin-arm64.tar.gz',
  );
  assert.equal(
    resolveReleaseArtifact({ platform: 'win32', arch: 'x64' }),
    'sem-windows-x86_64.tar.gz',
  );
});

test('getReleaseDownloadUrl respects a custom base url', () => {
  const url = getReleaseDownloadUrl('0.3.14', {
    platform: 'linux',
    arch: 'x64',
    env: {
      SEM_RELEASE_BASE_URL: 'https://example.com/releases/v0.3.14/',
    },
  });

  assert.equal(url, 'https://example.com/releases/v0.3.14/sem-linux-x86_64.tar.gz');
});

test('syncPackageVersion rewrites only the version field', async () => {
  const tempDirectory = await fs.mkdtemp(path.join(os.tmpdir(), 'sem-package-test-'));
  const packageJsonPath = path.join(tempDirectory, 'package.json');

  try {
    await fs.writeFile(
      packageJsonPath,
      `${JSON.stringify(
        {
          name: '@ataraxy-labs/sem',
          version: '0.0.0',
          private: true,
        },
        null,
        2,
      )}\n`,
      'utf8',
    );

    const result = await syncPackageVersion({
      packageRoot: tempDirectory,
      version: '1.2.3',
    });

    const packageJson = JSON.parse(await fs.readFile(packageJsonPath, 'utf8'));

    assert.equal(result.changed, true);
    assert.equal(result.version, '1.2.3');
    assert.equal(packageJson.version, '1.2.3');
    assert.equal(packageJson.name, '@ataraxy-labs/sem');
    assert.equal(packageJson.private, true);
  } finally {
    await fs.rm(tempDirectory, { recursive: true, force: true });
  }
});
