import { mkdirSync, existsSync } from 'node:fs';
import { resolve } from 'node:path';
import chalk from 'chalk';
import { SemDatabase } from '../../storage/database.js';
import { GitBridge } from '../../git/bridge.js';

export async function initCommand(opts: { cwd?: string } = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const repoRoot = await git.getRepoRoot();
  const semDir = resolve(repoRoot, '.sem');

  if (existsSync(semDir)) {
    console.log(chalk.yellow('.sem/ already exists. Reinitializing database...'));
  } else {
    mkdirSync(semDir, { recursive: true });
  }

  const dbPath = resolve(semDir, 'sem.db');
  const db = new SemDatabase(dbPath);

  db.setMetadata('version', '0.1.0');
  db.setMetadata('initialized_at', new Date().toISOString());

  const branch = await git.getCurrentBranch();
  db.setMetadata('branch', branch);

  db.close();

  console.log(chalk.green(`Initialized sem in ${semDir}`));
  console.log(chalk.dim(`  Database: ${dbPath}`));
  console.log(chalk.dim(`  Branch: ${branch}`));
}
