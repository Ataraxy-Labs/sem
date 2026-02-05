// Model types
export type { SemanticEntity } from './model/entity.js';
export type { SemanticChange, ChangeType } from './model/change.js';
export { buildEntityId } from './model/entity.js';
export { matchEntities, defaultSimilarity } from './model/identity.js';

// Storage
export { SemDatabase } from './storage/database.js';

// Git
export { GitBridge } from './git/bridge.js';
export type { DiffScope, FileChange, CommitInfo } from './git/types.js';

// Parser system
export type { SemanticParserPlugin } from './parser/plugin.js';
export { ParserRegistry } from './parser/registry.js';
export { computeSemanticDiff } from './parser/differ.js';
export type { DiffResult } from './parser/differ.js';
export { createDefaultRegistry } from './parser/plugins/index.js';

// Utilities
export { contentHash, shortHash } from './utils/hash.js';
export { normalizeFilePath, getExtension } from './utils/path.js';
