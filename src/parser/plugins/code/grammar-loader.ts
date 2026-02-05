import type { LanguageConfig } from './languages.js';

// Lazy-loaded grammar cache
const grammarCache = new Map<string, unknown>();

export function loadGrammar(config: LanguageConfig): unknown {
  if (grammarCache.has(config.id)) {
    return grammarCache.get(config.id)!;
  }

  try {
    // Use require for native tree-sitter grammars
    let grammar: unknown;

    if (config.grammarPackage === 'tree-sitter-typescript') {
      // tree-sitter-typescript exports { typescript, tsx }
      const pkg = require('tree-sitter-typescript');
      grammar = config.extensions.includes('.tsx') ? pkg.tsx : pkg.typescript;
      // Cache both variants
      grammarCache.set('typescript', pkg.typescript);
      grammarCache.set('tsx', pkg.tsx);
    } else {
      grammar = require(config.grammarPackage);
    }

    grammarCache.set(config.id, grammar);
    return grammar;
  } catch (err) {
    throw new Error(`Failed to load grammar for ${config.id}: ${(err as Error).message}`);
  }
}
