import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import type { SemanticEntity } from '../../model/entity.js';

export interface HistoryOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
  depth?: number;
}

interface HistoryEntry {
  commitSha: string;
  shortSha: string;
  author: string;
  date: string;
  message: string;
  changeType: 'added' | 'modified' | 'deleted' | 'unchanged';
  content?: string;
}

export async function historyCommand(entityQuery: string, opts: HistoryOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const repoRoot = await git.getRepoRoot();
  const registry = createDefaultRegistry();
  const depth = opts.depth ?? 50;

  // Parse entity query: "file.ts::function::name" or "file.ts::name" or just "name"
  let filePath: string | undefined;
  let entityName: string;

  if (entityQuery.includes('::')) {
    const parts = entityQuery.split('::');
    filePath = parts[0];
    entityName = parts[parts.length - 1];
  } else {
    entityName = entityQuery;
  }

  const commits = await git.getLog(depth);
  if (commits.length === 0) {
    console.log(chalk.dim('No commits found.'));
    return;
  }

  const timeline: HistoryEntry[] = [];
  let previousEntity: SemanticEntity | undefined;

  for (const commit of commits) {
    const simpleGit = (await import('simple-git')).default(repoRoot);

    // If we don't know the file, we need to search
    if (!filePath) {
      // Try to find entity in changed files of this commit
      const changedFiles = await git.getChangedFiles({ type: 'commit', sha: commit.sha });
      for (const file of changedFiles) {
        if (!file.afterContent && !file.beforeContent) continue;
        const content = file.afterContent ?? file.beforeContent;
        if (!content) continue;

        const plugin = registry.getPlugin(file.filePath);
        if (!plugin) continue;

        try {
          const entities = plugin.extractEntities(content, file.filePath);
          const found = entities.find(e => e.name === entityName);
          if (found) {
            filePath = file.filePath;
            break;
          }
        } catch { /* skip */ }
      }
      if (!filePath) continue;
    }

    // Get file content at this commit
    let contentAtCommit: string | undefined;
    try {
      contentAtCommit = await simpleGit.show([`${commit.sha}:${filePath}`]);
    } catch {
      // File doesn't exist at this commit
      if (previousEntity) {
        timeline.push({
          commitSha: commit.sha,
          shortSha: commit.shortSha,
          author: commit.author,
          date: commit.date,
          message: commit.message,
          changeType: 'deleted',
        });
        previousEntity = undefined;
      }
      continue;
    }

    const plugin = registry.getPlugin(filePath);
    if (!plugin || !contentAtCommit) continue;

    let entities: SemanticEntity[];
    try {
      entities = plugin.extractEntities(contentAtCommit, filePath);
    } catch {
      continue;
    }

    const entity = entities.find(e => e.name === entityName);
    if (!entity) {
      if (previousEntity) {
        timeline.push({
          commitSha: commit.sha,
          shortSha: commit.shortSha,
          author: commit.author,
          date: commit.date,
          message: commit.message,
          changeType: 'deleted',
        });
        previousEntity = undefined;
      }
      continue;
    }

    if (!previousEntity) {
      timeline.push({
        commitSha: commit.sha,
        shortSha: commit.shortSha,
        author: commit.author,
        date: commit.date,
        message: commit.message,
        changeType: 'added',
        content: entity.content,
      });
    } else if (previousEntity.contentHash !== entity.contentHash) {
      timeline.push({
        commitSha: commit.sha,
        shortSha: commit.shortSha,
        author: commit.author,
        date: commit.date,
        message: commit.message,
        changeType: 'modified',
        content: entity.content,
      });
    }

    previousEntity = entity;
  }

  // Reverse so oldest is first
  timeline.reverse();

  if (timeline.length === 0) {
    console.log(chalk.dim(`No history found for "${entityQuery}".`));
    return;
  }

  if (opts.format === 'json') {
    console.log(JSON.stringify({ entity: entityQuery, filePath, timeline }, null, 2));
    return;
  }

  // Terminal output
  console.log(chalk.bold(`\n  History: ${entityName}`) + chalk.dim(` in ${filePath}\n`));

  for (const entry of timeline) {
    const sha = chalk.yellow(entry.shortSha);
    const author = chalk.blue(entry.author);
    const date = chalk.dim(entry.date.split('T')[0]);

    const changeIcon =
      entry.changeType === 'added' ? chalk.green('⊕ created') :
      entry.changeType === 'modified' ? chalk.yellow('∆ modified') :
      chalk.red('⊖ deleted');

    console.log(`  ${sha} ${date} ${changeIcon} ${chalk.dim('by')} ${author}`);
    console.log(chalk.dim(`         ${entry.message}`));
    console.log('');
  }
}
