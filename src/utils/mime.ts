import { execFileSync } from 'node:child_process';

// Map MIME types returned by `file --mime-type` to parser plugin IDs
const MIME_TO_PLUGIN: Record<string, string> = {
  'application/json': 'json',
  'application/x-yaml': 'yaml',
  'text/x-yaml': 'yaml',
  'application/toml': 'toml',
  'application/typescript': 'code',
  'application/javascript': 'code',
  'text/x-python': 'code',
  'text/x-go': 'code',
  'text/x-rustsrc': 'code',
};

let fileCommandAvailable: boolean | undefined;

function isFileCommandAvailable(): boolean {
  if (fileCommandAvailable !== undefined) return fileCommandAvailable;
  try {
    execFileSync('file', ['--version'], { stdio: 'ignore' });
    fileCommandAvailable = true;
  } catch {
    fileCommandAvailable = false;
  }
  return fileCommandAvailable;
}

export function detectPluginByMime(absoluteFilePath: string): string | null {
  if (!isFileCommandAvailable()) return null;
  try {
    const mime = execFileSync('file', ['--mime-type', '-b', absoluteFilePath], {
      encoding: 'utf8',
      timeout: 2000,
    }).trim();
    return MIME_TO_PLUGIN[mime] ?? null;
  } catch {
    return null;
  }
}
