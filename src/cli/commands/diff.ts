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

export async function diffCommand(opts: DiffOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  // Determine scope
  let scope: DiffScope;
  if (opts.commit) {
    scope = { type: 'commit', sha: opts.commit };
  } else if (opts.from && opts.to) {
    scope = { type: 'range', from: opts.from, to: opts.to };
  } else if (opts.staged) {
    scope = { type: 'staged' };
  } else {
    scope = await git.detectScope();
  }

  // Get changed files with content
  const fileChanges = await git.getChangedFiles(scope);

  if (fileChanges.length === 0) {
    console.log(chalk.dim('No changes detected.'));
    return;
  }

  // Set up parser registry
  const registry = createDefaultRegistry();

  // Compute semantic diff
  const commitSha = scope.type === 'commit' ? scope.sha : undefined;
  const result = computeSemanticDiff(fileChanges, registry, commitSha);

  // Optionally store changes
  if (opts.store) {
    const repoRoot = await git.getRepoRoot();
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
    const repoRoot = await git.getRepoRoot();
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
