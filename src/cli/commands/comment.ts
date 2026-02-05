import { existsSync } from 'node:fs';
import { resolve } from 'node:path';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { SemDatabase } from '../../storage/database.js';

export interface CommentOptions {
  cwd?: string;
  author?: string;
  reply?: number;
  format?: 'terminal' | 'json';
}

export async function commentCommand(entityId: string, body: string | undefined, opts: CommentOptions = {}): Promise<void> {
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
    if (!body) {
      // List comments
      const comments = db.getComments(entityId);
      if (opts.format === 'json') {
        console.log(JSON.stringify({ entityId, comments }, null, 2));
        return;
      }

      if (comments.length === 0) {
        console.log(chalk.dim(`No comments on ${entityId}`));
        return;
      }

      console.log(chalk.bold(`\n  ${entityId}\n`));
      for (const c of comments) {
        const indent = c.parentId ? '      ' : '  ';
        const prefix = c.parentId ? chalk.dim('↳ ') : '';
        const author = c.author ? chalk.blue(c.author) : chalk.dim('anonymous');
        const date = chalk.dim(c.createdAt);
        console.log(`${indent}${prefix}${author} ${date}`);
        console.log(`${indent}  ${c.body}`);
        console.log('');
      }
      return;
    }

    // Add comment
    const author = opts.author ?? (await getGitAuthor(git));
    const id = db.addComment(entityId, body, author, opts.reply);
    console.log(chalk.green(`  Comment #${id} added to ${entityId}`));
  } finally {
    db.close();
  }
}

async function getGitAuthor(git: GitBridge): Promise<string> {
  try {
    const simpleGit = (await import('simple-git')).default(process.cwd());
    const config = await simpleGit.getConfig('user.name');
    return config.value ?? 'unknown';
  } catch {
    return 'unknown';
  }
}
