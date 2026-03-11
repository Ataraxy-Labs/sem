import chalk from 'chalk';
import type { DiffResult } from '../../parser/differ.js';
import type { SemanticChange } from '../../model/change.js';

const STATUS_LETTERS: Record<SemanticChange['changeType'], string> = {
  added: chalk.green('A'),
  modified: chalk.yellow('M'),
  deleted: chalk.red('D'),
  renamed: chalk.cyan('R'),
  moved: chalk.blue('>'),
};

export function formatPlaintext(result: DiffResult): string {
  if (result.changes.length === 0) {
    return 'No semantic changes detected.';
  }

  const lines: string[] = [];

  // Group changes by file
  const byFile = new Map<string, SemanticChange[]>();
  for (const change of result.changes) {
    const file = change.filePath;
    if (!byFile.has(file)) byFile.set(file, []);
    byFile.get(file)!.push(change);
  }

  for (const [filePath, changes] of byFile) {
    lines.push(chalk.bold(filePath));

    for (const change of changes) {
      const letter = STATUS_LETTERS[change.changeType];
      const typeLabel = change.entityType.padEnd(12);
      lines.push(`  ${letter}  ${chalk.dim(typeLabel)}${change.entityName}`);

      if ((change.changeType === 'renamed' || change.changeType === 'moved') && change.oldFilePath) {
        lines.push(`       ${chalk.dim(`from ${change.oldFilePath}`)}`);
      }
    }

    lines.push('');
  }

  // Summary
  const parts: string[] = [];
  if (result.addedCount > 0) parts.push(chalk.green(`${result.addedCount} added`));
  if (result.modifiedCount > 0) parts.push(chalk.yellow(`${result.modifiedCount} modified`));
  if (result.deletedCount > 0) parts.push(chalk.red(`${result.deletedCount} deleted`));
  if (result.movedCount > 0) parts.push(chalk.blue(`${result.movedCount} moved`));
  if (result.renamedCount > 0) parts.push(chalk.cyan(`${result.renamedCount} renamed`));

  lines.push(`${parts.join(', ')} across ${result.fileCount} file${result.fileCount !== 1 ? 's' : ''}`);

  return lines.join('\n');
}
