import { resolve } from 'node:path';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import type { SemanticEntity } from '../../model/entity.js';

export interface BlameOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
  depth?: number;
}

interface BlameEntry {
  entityId: string;
  entityType: string;
  entityName: string;
  filePath: string;
  author: string;
  commitSha: string;
  shortSha: string;
  date: string;
  message: string;
  startLine: number;
  endLine: number;
}

export async function blameCommand(filePath: string, opts: BlameOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const registry = createDefaultRegistry();
  const depth = opts.depth ?? 50;

  // Get current file content and extract entities
  const repoRoot = await git.getRepoRoot();
  const plugin = registry.getPlugin(filePath);
  if (!plugin) {
    console.error(chalk.red(`No parser available for ${filePath}`));
    process.exit(1);
  }

  const commits = await git.getLog(depth);
  if (commits.length === 0) {
    console.log(chalk.dim('No commits found.'));
    return;
  }

  // Get current entities from HEAD
  let currentContent: string;
  try {
    const { readFile } = await import('node:fs/promises');
    currentContent = await readFile(resolve(repoRoot, filePath), 'utf-8');
  } catch {
    console.error(chalk.red(`File not found: ${filePath}`));
    process.exit(1);
  }

  const currentEntities = plugin.extractEntities(currentContent, filePath);
  if (currentEntities.length === 0) {
    console.log(chalk.dim('No entities found in file.'));
    return;
  }

  // Walk commits to find who last touched each entity
  const blameMap = new Map<string, BlameEntry>();
  const unresolvedIds = new Set(currentEntities.map(e => e.id));

  for (let i = 0; i < commits.length && unresolvedIds.size > 0; i++) {
    const commit = commits[i];
    const nextCommit = commits[i + 1];

    let contentAtCommit: string | undefined;
    let contentBefore: string | undefined;

    try {
      const simpleGit = (await import('simple-git')).default(repoRoot);
      contentAtCommit = await simpleGit.show([`${commit.sha}:${filePath}`]);
    } catch {
      continue;
    }

    if (nextCommit) {
      try {
        const simpleGit = (await import('simple-git')).default(repoRoot);
        contentBefore = await simpleGit.show([`${nextCommit.sha}:${filePath}`]);
      } catch {
        contentBefore = undefined;
      }
    }

    if (!contentAtCommit) continue;

    let entitiesAtCommit: SemanticEntity[];
    try {
      entitiesAtCommit = plugin.extractEntities(contentAtCommit, filePath);
    } catch {
      continue;
    }

    let entitiesBefore: SemanticEntity[] = [];
    if (contentBefore) {
      try {
        entitiesBefore = plugin.extractEntities(contentBefore, filePath);
      } catch {
        entitiesBefore = [];
      }
    }

    const beforeById = new Map(entitiesBefore.map(e => [e.id, e]));
    const atCommitById = new Map(entitiesAtCommit.map(e => [e.id, e]));

    for (const entityId of [...unresolvedIds]) {
      const entityAtCommit = atCommitById.get(entityId);
      if (!entityAtCommit) continue;

      const entityBefore = beforeById.get(entityId);

      // Entity exists at this commit but not before → this commit introduced it
      // Entity exists at both but content differs → this commit modified it
      const isNew = !entityBefore;
      const isModified = entityBefore && entityBefore.contentHash !== entityAtCommit.contentHash;

      if (isNew || isModified || !nextCommit) {
        blameMap.set(entityId, {
          entityId,
          entityType: entityAtCommit.entityType,
          entityName: entityAtCommit.name,
          filePath,
          author: commit.author,
          commitSha: commit.sha,
          shortSha: commit.shortSha,
          date: commit.date,
          message: commit.message,
          startLine: entityAtCommit.startLine,
          endLine: entityAtCommit.endLine,
        });
        unresolvedIds.delete(entityId);
      }
    }
  }

  // Output
  const entries = currentEntities
    .map(e => blameMap.get(e.id))
    .filter((e): e is BlameEntry => e !== undefined);

  if (opts.format === 'json') {
    console.log(JSON.stringify(entries, null, 2));
    return;
  }

  // Terminal output
  console.log(chalk.dim(`\n  ${filePath}\n`));

  const maxNameLen = Math.max(...entries.map(e => e.entityName.length), 10);
  const maxAuthorLen = Math.max(...entries.map(e => e.author.length), 6);

  for (const entry of entries) {
    const sha = chalk.yellow(entry.shortSha);
    const author = chalk.blue(entry.author.padEnd(maxAuthorLen));
    const date = chalk.dim(entry.date.split('T')[0]);
    const type = chalk.dim(entry.entityType.padEnd(10));
    const name = chalk.bold(entry.entityName.padEnd(maxNameLen));
    const msg = chalk.dim(entry.message.slice(0, 40));

    console.log(`  ${sha} ${author} ${date} ${type} ${name} ${msg}`);
  }

  console.log('');
}
