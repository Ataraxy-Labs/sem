import type { SemanticParserPlugin } from '../../plugin.js';
import type { SemanticEntity } from '../../../model/entity.js';
import { contentHash } from '../../../utils/hash.js';
import { buildEntityId } from '../../../model/entity.js';

export class MarkdownParserPlugin implements SemanticParserPlugin {
  id = 'markdown';
  extensions = ['.md', '.mdx'];

  extractEntities(content: string, filePath: string): SemanticEntity[] {
    const entities: SemanticEntity[] = [];
    const lines = content.split('\n');

    interface Section {
      level: number;
      name: string;
      startLine: number;
      lines: string[];
      parentId?: string;
    }

    const sections: Section[] = [];
    let currentSection: Section | null = null;
    const sectionStack: Section[] = []; // Track nesting

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      const headingMatch = line.match(/^(#{1,6})\s+(.+)/);

      if (headingMatch) {
        // Close previous section
        if (currentSection) {
          sections.push(currentSection);
        }

        const level = headingMatch[1].length;
        const name = headingMatch[2].trim();

        // Find parent: walk up the stack to find a heading with lower level
        while (sectionStack.length > 0 && sectionStack[sectionStack.length - 1].level >= level) {
          sectionStack.pop();
        }

        const parentId = sectionStack.length > 0
          ? buildEntityId(filePath, 'heading', sectionStack[sectionStack.length - 1].name)
          : undefined;

        currentSection = {
          level,
          name,
          startLine: i + 1,
          lines: [line],
          parentId,
        };

        sectionStack.push(currentSection);
      } else if (currentSection) {
        currentSection.lines.push(line);
      } else {
        // Content before first heading — preamble
        if (line.trim()) {
          if (!currentSection) {
            currentSection = {
              level: 0,
              name: '(preamble)',
              startLine: i + 1,
              lines: [line],
            };
          } else {
            currentSection.lines.push(line);
          }
        }
      }
    }

    if (currentSection) {
      sections.push(currentSection);
    }

    for (const section of sections) {
      const sectionContent = section.lines.join('\n').trim();
      if (!sectionContent) continue;

      const entityType = section.level === 0 ? 'preamble' : 'heading';

      entities.push({
        id: buildEntityId(filePath, entityType, section.name),
        filePath,
        entityType,
        name: section.name,
        parentId: section.parentId,
        content: sectionContent,
        contentHash: contentHash(sectionContent),
        startLine: section.startLine,
        endLine: section.startLine + section.lines.length - 1,
      });
    }

    return entities;
  }
}
