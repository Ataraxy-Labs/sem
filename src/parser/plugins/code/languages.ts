export interface LanguageConfig {
  id: string;
  extensions: string[];
  grammarPackage: string;
  /** tree-sitter node types that represent top-level entities */
  entityNodeTypes: string[];
  /** Node types that can contain nested entities (e.g. class body) */
  containerNodeTypes: string[];
  /** How to extract the entity name from a node */
  nameExtractor: 'name_field' | 'declarator_name' | 'first_identifier';
}

export const LANGUAGE_CONFIGS: LanguageConfig[] = [
  {
    id: 'typescript',
    extensions: ['.ts', '.tsx'],
    grammarPackage: 'tree-sitter-typescript',
    entityNodeTypes: [
      'function_declaration',
      'class_declaration',
      'interface_declaration',
      'type_alias_declaration',
      'enum_declaration',
      'export_statement',
      'lexical_declaration',
      'variable_declaration',
      'method_definition',
      'public_field_definition',
      'pair',
    ],
    containerNodeTypes: ['class_body', 'interface_body', 'enum_body'],
    nameExtractor: 'name_field',
  },
  {
    id: 'javascript',
    extensions: ['.js', '.jsx', '.mjs', '.cjs'],
    grammarPackage: 'tree-sitter-javascript',
    entityNodeTypes: [
      'function_declaration',
      'class_declaration',
      'export_statement',
      'lexical_declaration',
      'variable_declaration',
      'method_definition',
      'field_definition',
      'pair',
    ],
    containerNodeTypes: ['class_body'],
    nameExtractor: 'name_field',
  },
  {
    id: 'python',
    extensions: ['.py'],
    grammarPackage: 'tree-sitter-python',
    entityNodeTypes: [
      'function_definition',
      'class_definition',
      'decorated_definition',
    ],
    containerNodeTypes: ['block'],
    nameExtractor: 'name_field',
  },
  {
    id: 'go',
    extensions: ['.go'],
    grammarPackage: 'tree-sitter-go',
    entityNodeTypes: [
      'function_declaration',
      'method_declaration',
      'type_declaration',
      'var_declaration',
      'const_declaration',
    ],
    containerNodeTypes: [],
    nameExtractor: 'name_field',
  },
  {
    id: 'rust',
    extensions: ['.rs'],
    grammarPackage: 'tree-sitter-rust',
    entityNodeTypes: [
      'function_item',
      'struct_item',
      'enum_item',
      'impl_item',
      'trait_item',
      'mod_item',
      'const_item',
      'static_item',
      'type_item',
    ],
    containerNodeTypes: ['declaration_list'],
    nameExtractor: 'name_field',
  },
];

export function getLanguageConfig(extension: string): LanguageConfig | undefined {
  return LANGUAGE_CONFIGS.find(c => c.extensions.includes(extension));
}

export function getAllCodeExtensions(): string[] {
  return LANGUAGE_CONFIGS.flatMap(c => c.extensions);
}
