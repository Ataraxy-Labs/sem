import type { SemanticEntity } from '../model/entity.js';

export interface SemanticParserPlugin {
  id: string;
  extensions: string[];
  extractEntities(content: string, filePath: string): SemanticEntity[];
  computeSimilarity?(a: SemanticEntity, b: SemanticEntity): number;
}
