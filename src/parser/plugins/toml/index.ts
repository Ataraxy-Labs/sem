import * as TOML from 'smol-toml';
import type { SemanticParserPlugin } from '../../plugin.js';
import type { SemanticEntity } from '../../../model/entity.js';
import { contentHash } from '../../../utils/hash.js';
import { buildEntityId } from '../../../model/entity.js';

export class TomlParserPlugin implements SemanticParserPlugin {
  id = 'toml';
  extensions = ['.toml'];

  extractEntities(content: string, filePath: string): SemanticEntity[] {
    const parsed = TOML.parse(content);
    const entities: SemanticEntity[] = [];
    this.walk(parsed, '', filePath, entities, content);
    return entities;
  }

  private walk(
    obj: Record<string, unknown>,
    path: string,
    filePath: string,
    entities: SemanticEntity[],
    fullContent: string,
    depth = 0,
  ): void {
    for (const [key, value] of Object.entries(obj)) {
      const dotPath = path ? `${path}.${key}` : key;
      const valueStr = typeof value === 'object' && value !== null
        ? JSON.stringify(value, null, 2)
        : String(value);

      const entityType = typeof value === 'object' && value !== null && !Array.isArray(value) ? 'section' : 'property';

      const lineMatch = findKeyLine(fullContent, key);

      entities.push({
        id: buildEntityId(filePath, entityType, dotPath),
        filePath,
        entityType,
        name: dotPath,
        parentId: path || undefined,
        content: valueStr,
        contentHash: contentHash(valueStr),
        startLine: lineMatch,
        endLine: lineMatch,
      });

      if (typeof value === 'object' && value !== null && !Array.isArray(value) && depth < 4) {
        this.walk(value as Record<string, unknown>, dotPath, filePath, entities, fullContent, depth + 1);
      }
    }
  }
}

function findKeyLine(content: string, key: string): number {
  const lines = content.split('\n');
  for (let i = 0; i < lines.length; i++) {
    const trimmed = lines[i].trimStart();
    if (trimmed.startsWith(`${key} =`) || trimmed.startsWith(`${key}=`) || trimmed === `[${key}]`) {
      return i + 1;
    }
  }
  return 0;
}
