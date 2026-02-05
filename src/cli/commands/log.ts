import { resolve } from 'node:path';
import { existsSync } from 'node:fs';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { ParserRegistry } from '../../parser/registry.js';
import { computeSemanticDiff } from '../../parser/differ.js';
import { SemDatabase } from '../../storage/database.js';
import { formatTerminal } from '../formatters/terminal.js';
import { formatJson } from '../formatters/json.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';

export interface LogOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
  count?: number;
  store?: boolean;
}

export async function logCommand(opts: LogOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const count = opts.count ?? 5;
  const commits = await git.getLog(count);

  if (commits.length === 0) {
    console.log(chalk.dim('No commits found.'));
    return;
  }

  const registry = createDefaultRegistry();
  const format = opts.format ?? 'terminal';

  const repoRoot = await git.getRepoRoot();
  const dbPath = resolve(repoRoot, '.sem', 'sem.db');
  const db = existsSync(dbPath) ? new SemDatabase(dbPath) : null;

  for (const commit of commits) {
    if (format === 'terminal') {
      console.log(chalk.yellow(`commit ${commit.sha}`));
      console.log(chalk.dim(`Author: ${commit.author}`));
      console.log(chalk.dim(`Date:   ${commit.date}`));
      console.log(`\n    ${commit.message}\n`);
    }

    const fileChanges = await git.getChangedFiles({
      type: 'commit',
      sha: commit.sha,
    });

    const result = computeSemanticDiff(fileChanges, registry, commit.sha, commit.author);

    if (opts.store && db) {
      db.insertChanges(result.changes);
    }

    if (format === 'json') {
      console.log(JSON.stringify({
        commit: {
          sha: commit.sha,
          author: commit.author,
          date: commit.date,
          message: commit.message,
        },
        summary: {
          fileCount: result.fileCount,
          added: result.addedCount,
          modified: result.modifiedCount,
          deleted: result.deletedCount,
          moved: result.movedCount,
          renamed: result.renamedCount,
          total: result.changes.length,
        },
        changes: result.changes,
      }, null, 2));
    } else {
      console.log(formatTerminal(result));
    }
  }

  db?.close();
}
