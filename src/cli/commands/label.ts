import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { SemDatabase } from '../../storage/database.js';

export interface LabelOptions {
  cwd?: string;
  remove?: boolean;
  list?: boolean;
  format?: 'terminal' | 'json';
}

export async function labelCommand(entityId: string, label: string | undefined, opts: LabelOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const repoRoot = await git.getRepoRoot();
  const dbPath = resolve(repoRoot, '.sem', 'sem.db');

  if (!existsSync(dbPath)) {
    console.error(chalk.red('No sem database found. Run `sem init` first.'));
    process.exit(1);
  }

  const db = new SemDatabase(dbPath);

  try {
    if (opts.list || !label) {
      const labels = db.getLabels(entityId);
      if (opts.format === 'json') {
        console.log(JSON.stringify({ entityId, labels }));
      } else if (labels.length === 0) {
        console.log(chalk.dim(`No labels on ${entityId}`));
      } else {
        console.log(chalk.bold(`  ${entityId}`));
        for (const l of labels) {
          console.log(`    ${chalk.cyan(l)}`);
        }
      }
      return;
    }

    if (opts.remove) {
      db.removeLabel(entityId, label);
      console.log(chalk.red(`  Removed label "${label}" from ${entityId}`));
    } else {
      db.addLabel(entityId, label);
      console.log(chalk.green(`  Added label "${label}" to ${entityId}`));
    }
  } finally {
    db.close();
  }
}
