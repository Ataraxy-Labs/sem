import type { SemanticParserPlugin } from '../../plugin.js';
import type { SemanticEntity } from '../../../model/entity.js';
import { contentHash } from '../../../utils/hash.js';
import { buildEntityId } from '../../../model/entity.js';

export class CsvParserPlugin implements SemanticParserPlugin {
  id = 'csv';
  extensions = ['.csv', '.tsv'];

  extractEntities(content: string, filePath: string): SemanticEntity[] {
    const entities: SemanticEntity[] = [];
    const lines = content.split('\n').filter(l => l.trim());
    if (lines.length === 0) return entities;

    const isTsv = filePath.endsWith('.tsv');
    const separator = isTsv ? '\t' : ',';

    // Parse header
    const headers = parseCsvLine(lines[0], separator);

    // Each row becomes an entity
    for (let i = 1; i < lines.length; i++) {
      const cells = parseCsvLine(lines[i], separator);
      const rowContent = lines[i];

      // Use first column as identity, or row number
      const rowId = cells[0] || `row_${i}`;
      const name = `row[${rowId}]`;

      entities.push({
        id: buildEntityId(filePath, 'row', name),
        filePath,
        entityType: 'row',
        name,
        content: rowContent,
        contentHash: contentHash(rowContent),
        startLine: i + 1,
        endLine: i + 1,
        metadata: Object.fromEntries(headers.map((h, idx) => [h, cells[idx] ?? ''])),
      });
    }

    return entities;
  }
}

function parseCsvLine(line: string, separator: string): string[] {
  const cells: string[] = [];
  let current = '';
  let inQuotes = false;

  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (inQuotes) {
      if (ch === '"' && line[i + 1] === '"') {
        current += '"';
        i++;
      } else if (ch === '"') {
        inQuotes = false;
      } else {
        current += ch;
      }
    } else {
      if (ch === '"') {
        inQuotes = true;
      } else if (ch === separator) {
        cells.push(current.trim());
        current = '';
      } else {
        current += ch;
      }
    }
  }
  cells.push(current.trim());
  return cells;
}
