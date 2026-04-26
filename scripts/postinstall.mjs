import fs from 'node:fs/promises';
import { createWriteStream } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import {
  getBinaryName,
  getInstalledBinaryPath,
  getReleaseBaseUrl,
  getReleaseDownloadUrl,
  readPackageVersion,
  resolveReleaseArtifact,
} from './package-meta.mjs';
import { verifyChecksum } from './verify-checksum.mjs';

async function downloadFile(url, destinationPath) {
  const response = await fetch(url, {
    headers: {
      'user-agent': '@ataraxy-labs/sem npm installer',
    },
    redirect: 'follow',
  });

  if (!response.ok) {
    throw new Error(`Download failed with ${response.status} ${response.statusText}`);
  }

  if (!response.body) {
    throw new Error('Download response did not include a body');
  }

  await pipeline(
    Readable.fromWeb(response.body),
    createWriteStream(destinationPath),
  );
}

function extractArchive(archivePath, outputDirectory) {
  const result = spawnSync('tar', ['-xzf', archivePath, '-C', outputDirectory], {
    stdio: 'pipe',
  });

  if (result.error) {
    throw new Error(`Failed to extract archive: ${result.error.message}`);
  }

  if (result.status !== 0) {
    const stderr = result.stderr?.toString('utf8').trim();
    throw new Error(
      `Failed to extract archive with tar${stderr ? `: ${stderr}` : ''}`,
    );
  }
}

async function installFromBinary(sourceBinaryPath, destinationBinaryPath) {
  await fs.mkdir(path.dirname(destinationBinaryPath), { recursive: true });
  await fs.copyFile(sourceBinaryPath, destinationBinaryPath);

  if (process.platform !== 'win32') {
    await fs.chmod(destinationBinaryPath, 0o755);
  }
}

async function installFromRelease(destinationBinaryPath) {
  const version = await readPackageVersion();
  const releaseBaseUrl = getReleaseBaseUrl(version);
  const releaseUrl = getReleaseDownloadUrl(version);
  const archiveName = resolveReleaseArtifact();
  const temporaryDirectory = await fs.mkdtemp(
    path.join(os.tmpdir(), 'sem-install-'),
  );

  try {
    const archivePath = path.join(temporaryDirectory, archiveName);
    await downloadFile(releaseUrl, archivePath);
    await verifyChecksum(archivePath, archiveName, releaseBaseUrl);
    extractArchive(archivePath, temporaryDirectory);
    await installFromBinary(
      path.join(temporaryDirectory, getBinaryName()),
      destinationBinaryPath,
    );
  } finally {
    await fs.rm(temporaryDirectory, { recursive: true, force: true });
  }
}

async function main() {
  if (process.env.SEM_SKIP_DOWNLOAD === '1') {
    console.log('Skipping sem binary download because SEM_SKIP_DOWNLOAD=1.');
    return;
  }

  const destinationBinaryPath = getInstalledBinaryPath();
  await fs.rm(destinationBinaryPath, { force: true });

  if (process.env.SEM_BINARY_PATH) {
    await installFromBinary(process.env.SEM_BINARY_PATH, destinationBinaryPath);
    console.log(`Installed sem from local binary ${process.env.SEM_BINARY_PATH}.`);
    return;
  }

  await installFromRelease(destinationBinaryPath);
  console.log(`Installed sem binary to ${destinationBinaryPath}.`);
}

main().catch((error) => {
  console.error(`Failed to install sem: ${error.message}`);
  process.exit(1);
});
