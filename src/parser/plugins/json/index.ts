import type { SemanticParserPlugin } from '../../plugin.js';
import type { SemanticEntity } from '../../../model/entity.js';
import { contentHash } from '../../../utils/hash.js';
import { buildEntityId } from '../../../model/entity.js';

export class JsonParserPlugin implements SemanticParserPlugin {
  id = 'json';
  extensions = ['.json'];

  extractEntities(content: string, filePath: string): SemanticEntity[] {
    const parsed = JSON.parse(content);
    const entities: SemanticEntity[] = [];
    this.walk(parsed, '', filePath, entities, content);
    return entities;
  }

  private walk(
    value: unknown,
    pointer: string,
    filePath: string,
    entities: SemanticEntity[],
    fullContent: string,
    depth = 0,
  ): void {
    if (value === null || value === undefined) return;

    if (Array.isArray(value)) {
      // For arrays, extract each element as an entity
      for (let i = 0; i < value.length; i++) {
        const itemPointer = `${pointer}/${i}`;
        const itemContent = JSON.stringify(value[i], null, 2);
        const name = `[${i}]`;

        // Only create entities for non-primitive array items
        if (typeof value[i] === 'object' && value[i] !== null) {
          entities.push({
            id: buildEntityId(filePath, 'element', itemPointer),
            filePath,
            entityType: 'element',
            name,
            parentId: pointer || undefined,
            content: itemContent,
            contentHash: contentHash(itemContent),
            startLine: 0,
            endLine: 0,
          });

          if (depth < 3) {
            this.walk(value[i], itemPointer, filePath, entities, fullContent, depth + 1);
          }
        }
      }
    } else if (typeof value === 'object') {
      const entries = Object.entries(value as Record<string, unknown>);

      for (const [key, val] of entries) {
        const escapedKey = key.replace(/~/g, '~0').replace(/\//g, '~1');
        const propPointer = `${pointer}/${escapedKey}`;
        const propContent = JSON.stringify(val, null, 2);
        const entityType = typeof val === 'object' && val !== null ? 'object' : 'property';
        const name = key;

        entities.push({
          id: buildEntityId(filePath, entityType, propPointer),
          filePath,
          entityType,
          name,
          parentId: pointer || undefined,
          content: propContent,
          contentHash: contentHash(propContent),
          startLine: 0,
          endLine: 0,
        });

        if (typeof val === 'object' && val !== null && depth < 3) {
          this.walk(val, propPointer, filePath, entities, fullContent, depth + 1);
        }
      }
    }
  }
}
