import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { verifyChecksum } from './verify-checksum.mjs';

function sha256(contents) {
  return createHash('sha256').update(contents).digest('hex');
}

function checksumResponse(text, options = {}) {
  return {
    ok: options.ok ?? true,
    status: options.status ?? 200,
    statusText: options.statusText ?? 'OK',
    async text() {
      return text;
    },
  };
}

async function withArchive(contents, callback) {
  const tempDirectory = await fs.mkdtemp(path.join(os.tmpdir(), 'sem-checksum-test-'));
  const archiveName = 'sem-linux-x86_64.tar.gz';
  const archivePath = path.join(tempDirectory, archiveName);

  try {
    await fs.writeFile(archivePath, contents);
    return await callback({ archiveName, archivePath });
  } finally {
    await fs.rm(tempDirectory, { recursive: true, force: true });
  }
}

test('verifyChecksum accepts a matching checksum entry', async () => {
  await withArchive('release archive', async ({ archiveName, archivePath }) => {
    const expectedHash = sha256('release archive');

    await assert.doesNotReject(() =>
      verifyChecksum(archivePath, archiveName, 'https://example.com/releases/v1', {
        fetchFn: async () => checksumResponse(`${expectedHash}  ${archiveName}\n`),
      }),
    );
  });
});

test('verifyChecksum fails closed when checksum metadata cannot be fetched', async () => {
  await withArchive('release archive', async ({ archiveName, archivePath }) => {
    await assert.rejects(
      () =>
        verifyChecksum(archivePath, archiveName, 'https://example.com/releases/v1', {
          fetchFn: async () =>
            checksumResponse('', {
              ok: false,
              status: 503,
              statusText: 'Service Unavailable',
            }),
        }),
      /Failed to fetch checksum metadata.*503 Service Unavailable/,
    );
  });
});

test('verifyChecksum fails closed when checksum metadata fetch rejects', async () => {
  await withArchive('release archive', async ({ archiveName, archivePath }) => {
    await assert.rejects(
      () =>
        verifyChecksum(archivePath, archiveName, 'https://example.com/releases/v1', {
          fetchFn: async () => {
            throw new Error('network offline');
          },
        }),
      /Failed to fetch checksum metadata.*network offline/,
    );
  });
});

test('verifyChecksum fails closed when archive has no checksum entry', async () => {
  await withArchive('release archive', async ({ archiveName, archivePath }) => {
    const otherHash = sha256('other archive');

    await assert.rejects(
      () =>
        verifyChecksum(archivePath, archiveName, 'https://example.com/releases/v1', {
          fetchFn: async () => checksumResponse(`${otherHash}  sem-darwin-arm64.tar.gz\n`),
        }),
      /No checksum found for sem-linux-x86_64\.tar\.gz/,
    );
  });
});

test('verifyChecksum rejects checksum mismatches', async () => {
  await withArchive('release archive', async ({ archiveName, archivePath }) => {
    const expectedHash = sha256('tampered archive');

    await assert.rejects(
      () =>
        verifyChecksum(archivePath, archiveName, 'https://example.com/releases/v1', {
          fetchFn: async () => checksumResponse(`${expectedHash}  ${archiveName}\n`),
        }),
      /Checksum mismatch for sem-linux-x86_64\.tar\.gz/,
    );
  });
});

test('verifyChecksum skips verification when SEM_SKIP_CHECKSUM=1', async () => {
  await withArchive('release archive', async ({ archiveName, archivePath }) => {
    let fetched = false;
    const originalWarn = console.warn;

    try {
      console.warn = () => {};

      await assert.doesNotReject(() =>
        verifyChecksum(archivePath, archiveName, 'https://example.com/releases/v1', {
          env: { SEM_SKIP_CHECKSUM: '1' },
          fetchFn: async () => {
            fetched = true;
            throw new Error('fetch should not be called');
          },
        }),
      );
    } finally {
      console.warn = originalWarn;
    }

    assert.equal(fetched, false);
  });
});
