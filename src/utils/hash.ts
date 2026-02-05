import { createHash } from 'node:crypto';

export function contentHash(content: string): string {
  return createHash('sha256').update(content).digest('hex');
}

export function shortHash(content: string, length = 8): string {
  return contentHash(content).slice(0, length);
}
