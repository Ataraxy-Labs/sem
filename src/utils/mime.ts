import { execFileSync } from 'node:child_process';

// Map MIME types returned by `file --mime-type` to parser plugin IDs
const MIME_TO_PLUGIN: Record<string, string> = {
  // JSON
  'application/json': 'json',
  // YAML
  'application/x-yaml': 'yaml',
  'text/x-yaml': 'yaml',
  'text/yaml': 'yaml',
  // TOML
  'application/toml': 'toml',
  // CSV
  'text/csv': 'csv',
  // Markdown
  'text/markdown': 'markdown',
  'text/x-markdown': 'markdown',
  // Code (TypeScript, JavaScript, Python, Go, Rust)
  'application/typescript': 'code',
  'text/typescript': 'code',
  'text/x-typescript': 'code',
  'application/javascript': 'code',
  'text/javascript': 'code',
  'text/x-python': 'code',
  'text/x-script.python': 'code',
  'text/x-go': 'code',
  'text/x-rustsrc': 'code',
};

let fileCommandAvailable: boolean | undefined;
const mimeCache = new Map<string, string | null>();

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
  if (mimeCache.has(absoluteFilePath)) return mimeCache.get(absoluteFilePath)!;
  try {
    const mime = execFileSync('file', ['--mime-type', '-b', absoluteFilePath], {
      encoding: 'utf8',
      timeout: 2000,
    }).trim();
    const result = MIME_TO_PLUGIN[mime] ?? null;
    mimeCache.set(absoluteFilePath, result);
    return result;
  } catch {
    mimeCache.set(absoluteFilePath, null);
    return null;
  }
}
