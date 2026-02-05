import chalk from 'chalk';
import type { DiffResult } from '../../parser/differ.js';
import type { SemanticChange } from '../../model/change.js';

const SYMBOLS = {
  added: chalk.green('⊕'),
  modified: chalk.yellow('∆'),
  deleted: chalk.red('⊖'),
  moved: chalk.blue('→'),
  renamed: chalk.cyan('↻'),
};

const COLORS = {
  added: chalk.green,
  modified: chalk.yellow,
  deleted: chalk.red,
  moved: chalk.blue,
  renamed: chalk.cyan,
};

export function formatTerminal(result: DiffResult): string {
  if (result.changes.length === 0) {
    return chalk.dim('No semantic changes detected.');
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
    const header = `─ ${filePath} `;
    const padLen = Math.max(0, 55 - header.length);
    lines.push(chalk.dim(`┌${header}${'─'.repeat(padLen)}`));
    lines.push(chalk.dim('│'));

    for (const change of changes) {
      const symbol = SYMBOLS[change.changeType];
      const color = COLORS[change.changeType];
      const typeLabel = change.entityType.padEnd(10);
      const tag = color(`[${change.changeType}]`);

      lines.push(chalk.dim('│  ') + `${symbol} ${chalk.dim(typeLabel)} ${chalk.bold(change.entityName).padEnd(25)} ${tag}`);

      // Show content diff for modified properties
      if (change.changeType === 'modified' && change.beforeContent && change.afterContent) {
        const before = change.beforeContent.split('\n');
        const after = change.afterContent.split('\n');

        // Only show inline diff for short content
        if (before.length <= 3 && after.length <= 3) {
          for (const line of before) {
            lines.push(chalk.dim('│    ') + chalk.red(`- ${line.trim()}`));
          }
          for (const line of after) {
            lines.push(chalk.dim('│    ') + chalk.green(`+ ${line.trim()}`));
          }
        }
      }

      // Show rename/move details
      if ((change.changeType === 'renamed' || change.changeType === 'moved') && change.oldFilePath) {
        lines.push(chalk.dim('│    ') + chalk.dim(`from ${change.oldFilePath}`));
      }
    }

    lines.push(chalk.dim('│'));
    lines.push(chalk.dim('└' + '─'.repeat(55)));
    lines.push('');
  }

  // Summary
  const parts: string[] = [];
  if (result.addedCount > 0) parts.push(chalk.green(`${result.addedCount} added`));
  if (result.modifiedCount > 0) parts.push(chalk.yellow(`${result.modifiedCount} modified`));
  if (result.deletedCount > 0) parts.push(chalk.red(`${result.deletedCount} deleted`));
  if (result.movedCount > 0) parts.push(chalk.blue(`${result.movedCount} moved`));
  if (result.renamedCount > 0) parts.push(chalk.cyan(`${result.renamedCount} renamed`));

  lines.push(`Summary: ${parts.join(', ')} across ${result.fileCount} file${result.fileCount !== 1 ? 's' : ''}`);

  return lines.join('\n');
}
