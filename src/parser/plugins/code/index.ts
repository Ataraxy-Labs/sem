import type { SemanticParserPlugin } from '../../plugin.js';
import type { SemanticEntity } from '../../../model/entity.js';
import { getAllCodeExtensions, getLanguageConfig } from './languages.js';
import { loadGrammar } from './grammar-loader.js';
import { extractEntities } from './entity-extractor.js';
import { defaultSimilarity } from '../../../model/identity.js';
import { getExtension } from '../../../utils/path.js';

// Lazy-loaded Parser
let Parser: any = null;

function getParser(): any {
  if (!Parser) {
    try {
      Parser = require('tree-sitter');
    } catch {
      return null;
    }
  }
  return Parser;
}

export class CodeParserPlugin implements SemanticParserPlugin {
  id = 'code';
  extensions = getAllCodeExtensions();

  extractEntities(content: string, filePath: string): SemanticEntity[] {
    const ParserClass = getParser();
    if (!ParserClass) {
      return []; // tree-sitter not available, skip
    }

    const ext = getExtension(filePath);
    const config = getLanguageConfig(ext);
    if (!config) return [];

    let grammar: unknown;
    try {
      grammar = loadGrammar(config, ext);
    } catch {
      return []; // Grammar not installed
    }

    const parser = new ParserClass();
    parser.setLanguage(grammar);

    const tree = parser.parse(content);
    return extractEntities(tree, filePath, config, content);
  }

  computeSimilarity(a: SemanticEntity, b: SemanticEntity): number {
    return defaultSimilarity(a, b);
  }
}
