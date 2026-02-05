import { resolve, relative, extname } from 'node:path';

export function normalizeFilePath(filePath: string, repoRoot: string): string {
  const abs = resolve(repoRoot, filePath);
  return relative(repoRoot, abs);
}

export function getExtension(filePath: string): string {
  return extname(filePath).toLowerCase();
}
