import type { LanguageConfig } from './languages.js';

// Lazy-loaded grammar cache
const grammarCache = new Map<string, unknown>();

export function loadGrammar(config: LanguageConfig, extension: string): unknown {
  const cacheKey = config.grammarPackage === 'tree-sitter-typescript'
    ? (extension === '.tsx' ? 'tsx' : 'typescript')
    : config.id;

  if (grammarCache.has(cacheKey)) {
    return grammarCache.get(cacheKey)!;
  }

  try {
    // Use require for native tree-sitter grammars
    let grammar: unknown;

    if (config.grammarPackage === 'tree-sitter-typescript') {
      // tree-sitter-typescript exports { typescript, tsx }
      const pkg = require('tree-sitter-typescript');
      grammar = extension === '.tsx' ? pkg.tsx : pkg.typescript;
      // Cache both variants
      grammarCache.set('typescript', pkg.typescript);
      grammarCache.set('tsx', pkg.tsx);
    } else {
      grammar = require(config.grammarPackage);
    }

    grammarCache.set(cacheKey, grammar);
    return grammar;
  } catch (err) {
    throw new Error(`Failed to load grammar for ${config.id} (${extension}): ${(err as Error).message}`);
  }
}
