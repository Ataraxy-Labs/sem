import { resolve } from 'node:path';
import { existsSync } from 'node:fs';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { SemDatabase } from '../../storage/database.js';

export interface QueryOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
}

export async function queryCommand(sql: string, opts: QueryOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const repoRoot = await git.getRepoRoot();
  const dbPath = resolve(repoRoot, '.sem', 'sem.db');

  if (!existsSync(dbPath)) {
    console.error(chalk.red('Error: No sem database found. Run `sem init` first.'));
    process.exit(1);
  }

  const db = new SemDatabase(dbPath);

  try {
    const results = db.query(sql);

    if (opts.format === 'json') {
      console.log(JSON.stringify(results, null, 2));
    } else {
      if (results.length === 0) {
        console.log(chalk.dim('No results.'));
      } else {
        // Print as table
        const columns = Object.keys(results[0] as Record<string, unknown>);
        const header = columns.map(c => c.padEnd(20)).join(' │ ');
        console.log(chalk.bold(header));
        console.log('─'.repeat(header.length));

        for (const row of results as Array<Record<string, unknown>>) {
          const line = columns.map(c => String(row[c] ?? '').slice(0, 20).padEnd(20)).join(' │ ');
          console.log(line);
        }

        console.log(chalk.dim(`\n${results.length} row${results.length !== 1 ? 's' : ''}`));
      }
    }
  } catch (err) {
    console.error(chalk.red(`SQL Error: ${(err as Error).message}`));
    process.exit(1);
  } finally {
    db.close();
  }
}
