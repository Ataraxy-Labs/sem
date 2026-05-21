import { createHash } from 'node:crypto';
import fs from 'node:fs/promises';

/**
 * Downloads checksums.txt from the release, verifies the archive matches.
 * Returns silently on success, throws on mismatch or missing checksum.
 */
export async function verifyChecksum(
  archivePath,
  archiveName,
  releaseBaseUrl,
  { env = process.env, fetchFn = fetch } = {},
) {
  if (env.SEM_SKIP_CHECKSUM === '1') {
    console.warn('Skipping checksum verification because SEM_SKIP_CHECKSUM=1.');
    return;
  }

  const checksumsUrl = `${releaseBaseUrl}/checksums.txt`;

  let response;
  try {
    response = await fetchFn(checksumsUrl, {
      headers: { 'user-agent': '@ataraxy-labs/sem npm installer' },
      redirect: 'follow',
    });
  } catch (error) {
    throw new Error(
      `Failed to fetch checksum metadata from ${checksumsUrl}: ${error.message}`,
    );
  }

  if (!response.ok) {
    throw new Error(
      `Failed to fetch checksum metadata from ${checksumsUrl}: ` +
        `${response.status} ${response.statusText}`,
    );
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
    throw new Error(
      `No checksum found for ${archiveName} in checksum metadata from ${checksumsUrl}.`,
    );
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
