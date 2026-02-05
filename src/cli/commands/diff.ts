import { resolve } from 'node:path';
import { existsSync } from 'node:fs';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import type { DiffScope } from '../../git/types.js';
import { ParserRegistry } from '../../parser/registry.js';
import { computeSemanticDiff } from '../../parser/differ.js';
import { SemDatabase } from '../../storage/database.js';
import { formatTerminal } from '../formatters/terminal.js';
import { formatJson } from '../formatters/json.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import { loadConfig, validateChanges, formatValidationResults } from './validate.js';

export interface DiffOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
  staged?: boolean;
  commit?: string;
  from?: string;
  to?: string;
  store?: boolean;
}

// Singleton registry — no need to recreate on every call
let _registry: ParserRegistry | undefined;
function getRegistry(): ParserRegistry {
  return (_registry ??= createDefaultRegistry());
}

export async function diffCommand(opts: DiffOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  let scope: DiffScope;
  let fileChanges;

  // Fast path: auto-detect uses combined detectAndGetFiles (1 round of git calls)
  // Explicit scope falls back to separate calls
  if (opts.commit) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    scope = { type: 'commit', sha: opts.commit };
    fileChanges = await git.getChangedFiles(scope);
  } else if (opts.from && opts.to) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    scope = { type: 'range', from: opts.from, to: opts.to };
    fileChanges = await git.getChangedFiles(scope);
  } else if (opts.staged) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    scope = { type: 'staged' };
    fileChanges = await git.getChangedFiles(scope);
  } else {
    // Combined: isRepo + detectScope + getChangedFiles in one batch
    try {
      const result = await git.detectAndGetFiles();
      scope = result.scope;
      fileChanges = result.files;
    } catch {
      console.error(chalk.red('Error: Not inside a Git repository.'));
      process.exit(1);
    }
  }

  if (fileChanges.length === 0) {
    console.log(chalk.dim('No changes detected.'));
    return;
  }

  // Compute semantic diff
  const registry = getRegistry();
  const commitSha = scope.type === 'commit' ? scope.sha : undefined;
  const result = computeSemanticDiff(fileChanges, registry, commitSha);

  // Get repoRoot once for both store + validation
  const repoRoot = await git.getRepoRoot();

  // Optionally store changes
  if (opts.store) {
    const dbPath = resolve(repoRoot, '.sem', 'sem.db');
    if (existsSync(dbPath)) {
      const db = new SemDatabase(dbPath);
      db.insertChanges(result.changes);
      db.close();
    }
  }

  // Output
  const format = opts.format ?? 'terminal';
  if (format === 'json') {
    console.log(formatJson(result));
  } else {
    console.log(formatTerminal(result));
  }

  // Run validation rules if .semrc exists
  try {
    const config = await loadConfig(repoRoot);
    if (config.rules && config.rules.length > 0) {
      const violations = validateChanges(result, config);
      if (violations.length > 0) {
        console.log('');
        console.log(formatValidationResults(violations));
      }
    }
  } catch {
    // No config or invalid config — skip validation
  }
}
