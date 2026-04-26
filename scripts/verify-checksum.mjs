import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';

/**
 * Downloads checksums.txt from the release, verifies the archive matches.
 * Returns silently on success, throws on mismatch or missing checksum.
 */
export async function verifyChecksum(archivePath, archiveName, releaseBaseUrl) {
  const checksumsUrl = `${releaseBaseUrl}/checksums.txt`;

  const response = await fetch(checksumsUrl, {
    headers: { 'user-agent': '@ataraxy-labs/sem npm installer' },
    redirect: 'follow',
  });

  if (!response.ok) {
    console.warn(
      `Could not fetch checksums (${response.status}), skipping verification.`,
    );
    return;
  }

  const checksumsText = await response.text();
  const lines = checksumsText.trim().split('\n');

  let expectedHash = null;
  for (const line of lines) {
    const [hash, filename] = line.split(/\s+/);
    if (filename === archiveName) {
      expectedHash = hash;
      break;
    }
  }

  if (!expectedHash) {
    console.warn(
      `No checksum found for ${archiveName} in checksums.txt, skipping verification.`,
    );
    return;
  }

  const fileBuffer = await fs.readFile(archivePath);
  const actualHash = createHash('sha256').update(fileBuffer).digest('hex');

  if (actualHash !== expectedHash) {
    throw new Error(
      `Checksum mismatch for ${archiveName}.\n` +
        `  Expected: ${expectedHash}\n` +
        `  Actual:   ${actualHash}\n` +
        `The downloaded file may be corrupted or tampered with.`,
    );
  }
}
