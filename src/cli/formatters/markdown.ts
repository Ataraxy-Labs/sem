import type { DiffResult } from '../../parser/differ.js';
import type { SemanticChange } from '../../model/change.js';

const SYMBOLS = {
  added: '+',
  modified: 'Δ',
  deleted: '-',
  moved: '→',
  renamed: '↻',
};

export function formatMarkdown(result: DiffResult): string {
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
    lines.push(`### ${filePath}`);
    lines.push('');
    lines.push('| Status | Type | Name |');
    lines.push('|--------|------|------|');

    for (const change of changes) {
      const symbol = SYMBOLS[change.changeType];
      lines.push(`| ${symbol} | ${change.entityType} | ${change.entityName} |`);
    }

    lines.push('');

    // Show content diff for modified entities with short content
    for (const change of changes) {
      if (change.changeType === 'modified' && change.beforeContent && change.afterContent) {
        const before = change.beforeContent.split('\n');
        const after = change.afterContent.split('\n');

        if (before.length <= 3 && after.length <= 3) {
          lines.push('```diff');
          for (const line of before) {
            lines.push(`- ${line.trim()}`);
          }
          for (const line of after) {
            lines.push(`+ ${line.trim()}`);
          }
          lines.push('```');
          lines.push('');
        }
      }

      // Show rename/move details
      if ((change.changeType === 'renamed' || change.changeType === 'moved') && change.oldFilePath) {
        lines.push(`from ${change.oldFilePath}`);
        lines.push('');
      }
    }
  }

  // Summary
  const parts: string[] = [];
  if (result.addedCount > 0) parts.push(`${result.addedCount} added`);
  if (result.modifiedCount > 0) parts.push(`${result.modifiedCount} modified`);
  if (result.deletedCount > 0) parts.push(`${result.deletedCount} deleted`);
  if (result.movedCount > 0) parts.push(`${result.movedCount} moved`);
  if (result.renamedCount > 0) parts.push(`${result.renamedCount} renamed`);

  lines.push(`**Summary:** ${parts.join(', ')} across ${result.fileCount} file${result.fileCount !== 1 ? 's' : ''}`);

  return lines.join('\n');
}
