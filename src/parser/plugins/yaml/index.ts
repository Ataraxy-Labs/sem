import yaml from 'js-yaml';
import type { SemanticParserPlugin } from '../../plugin.js';
import type { SemanticEntity } from '../../../model/entity.js';
import { contentHash } from '../../../utils/hash.js';
import { buildEntityId } from '../../../model/entity.js';

export class YamlParserPlugin implements SemanticParserPlugin {
  id = 'yaml';
  extensions = ['.yml', '.yaml'];

  extractEntities(content: string, filePath: string): SemanticEntity[] {
    const parsed = yaml.load(content);
    if (typeof parsed !== 'object' || parsed === null) return [];

    const entities: SemanticEntity[] = [];
    this.walk(parsed as Record<string, unknown>, '', filePath, entities, content);
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
        ? yaml.dump(value).trim()
        : String(value);

      const entityType = typeof value === 'object' && value !== null ? 'section' : 'property';

      // Approximate line numbers by searching for the key
      const lineMatch = findKeyLine(fullContent, key, path);

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

function findKeyLine(content: string, key: string, parentPath: string): number {
  const lines = content.split('\n');
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trimStart().startsWith(`${key}:`)) {
      return i + 1;
    }
  }
  return 0;
}
