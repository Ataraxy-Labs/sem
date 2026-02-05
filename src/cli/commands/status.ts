import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import { computeSemanticDiff } from '../../parser/differ.js';
import type { SemanticChange } from '../../model/change.js';

export interface StatusOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
}

export async function statusCommand(opts: StatusOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const scope = await git.detectScope();
  const fileChanges = await git.getChangedFiles(scope);

  if (fileChanges.length === 0) {
    console.log(chalk.dim('Nothing changed.'));
    return;
  }

  const registry = createDefaultRegistry();
  const result = computeSemanticDiff(fileChanges, registry);

  if (opts.format === 'json') {
    // Group by file
    const byFile = new Map<string, SemanticChange[]>();
    for (const change of result.changes) {
      if (!byFile.has(change.filePath)) byFile.set(change.filePath, []);
      byFile.get(change.filePath)!.push(change);
    }

    const output = {
      scope: scope.type,
      summary: {
        files: result.fileCount,
        added: result.addedCount,
        modified: result.modifiedCount,
        deleted: result.deletedCount,
        moved: result.movedCount,
        renamed: result.renamedCount,
        total: result.changes.length,
      },
      files: Object.fromEntries(
        [...byFile].map(([file, changes]) => [
          file,
          {
            added: changes.filter(c => c.changeType === 'added').length,
            modified: changes.filter(c => c.changeType === 'modified').length,
            deleted: changes.filter(c => c.changeType === 'deleted').length,
            entities: changes.map(c => ({
              name: c.entityName,
              type: c.entityType,
              change: c.changeType,
            })),
          },
        ])
      ),
    };
    console.log(JSON.stringify(output, null, 2));
    return;
  }

  // Terminal output
  const scopeLabel = scope.type === 'working' ? 'working directory' :
    scope.type === 'staged' ? 'staged changes' :
    scope.type === 'commit' ? `commit ${(scope as any).sha.slice(0, 7)}` :
    `${(scope as any).from}..${(scope as any).to}`;

  console.log(chalk.dim(`\n  On ${await git.getCurrentBranch()} · ${scopeLabel}\n`));

  // Group by file
  const byFile = new Map<string, SemanticChange[]>();
  for (const change of result.changes) {
    if (!byFile.has(change.filePath)) byFile.set(change.filePath, []);
    byFile.get(change.filePath)!.push(change);
  }

  for (const [filePath, changes] of byFile) {
    const added = changes.filter(c => c.changeType === 'added').length;
    const modified = changes.filter(c => c.changeType === 'modified').length;
    const deleted = changes.filter(c => c.changeType === 'deleted').length;
    const moved = changes.filter(c => c.changeType === 'moved').length;
    const renamed = changes.filter(c => c.changeType === 'renamed').length;

    const parts: string[] = [];
    if (added > 0) parts.push(chalk.green(`+${added}`));
    if (modified > 0) parts.push(chalk.yellow(`~${modified}`));
    if (deleted > 0) parts.push(chalk.red(`-${deleted}`));
    if (moved > 0) parts.push(chalk.blue(`→${moved}`));
    if (renamed > 0) parts.push(chalk.cyan(`↻${renamed}`));

    const summary = parts.join(' ');

    // Collect entity types
    const types = new Map<string, number>();
    for (const c of changes) {
      types.set(c.entityType, (types.get(c.entityType) ?? 0) + 1);
    }
    const typeStr = [...types].map(([t, n]) => `${n} ${t}${n > 1 ? 's' : ''}`).join(', ');

    console.log(`  ${chalk.bold(filePath)}  ${summary}`);
    console.log(chalk.dim(`    ${typeStr}`));
  }

  console.log('');

  // Total summary
  const total = result.changes.length;
  console.log(chalk.dim(`  ${total} entities changed across ${result.fileCount} files`));
  console.log('');
}
