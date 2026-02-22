import type { SemanticEntity } from '../../../model/entity.js';
import { buildEntityId } from '../../../model/entity.js';
import { contentHash } from '../../../utils/hash.js';
import type { LanguageConfig } from './languages.js';

interface TreeSitterNode {
  type: string;
  text: string;
  startPosition: { row: number; column: number };
  endPosition: { row: number; column: number };
  childCount: number;
  children: TreeSitterNode[];
  childForFieldName(name: string): TreeSitterNode | null;
  namedChildren: TreeSitterNode[];
}

interface TreeSitterTree {
  rootNode: TreeSitterNode;
}

export function extractEntities(
  tree: TreeSitterTree,
  filePath: string,
  config: LanguageConfig,
  sourceCode: string,
): SemanticEntity[] {
  const entities: SemanticEntity[] = [];
  visitNode(tree.rootNode, filePath, config, entities, undefined, sourceCode);
  return entities;
}

function visitNode(
  node: TreeSitterNode,
  filePath: string,
  config: LanguageConfig,
  entities: SemanticEntity[],
  parentId: string | undefined,
  sourceCode: string,
): void {
  if (config.entityNodeTypes.includes(node.type)) {
    const name = extractName(node, config, sourceCode);
    const entityType = mapNodeType(node.type);
    const content = node.text;

    if (name) {
      const entity: SemanticEntity = {
        id: buildEntityId(filePath, entityType, name, parentId),
        filePath,
        entityType,
        name,
        parentId,
        content,
        contentHash: contentHash(content),
        startLine: node.startPosition.row + 1,
        endLine: node.endPosition.row + 1,
      };

      entities.push(entity);

      // Visit children for nested entities (methods inside classes, etc.)
      for (const child of node.namedChildren) {
        if (config.containerNodeTypes.includes(child.type)) {
          for (const nested of child.namedChildren) {
            visitNode(nested, filePath, config, entities, entity.id, sourceCode);
          }
        }
      }
      return;
    }
  }

  // For export statements, look inside for the actual declaration
  if (node.type === 'export_statement') {
    const declaration = node.childForFieldName('declaration');
    if (declaration) {
      visitNode(declaration, filePath, config, entities, parentId, sourceCode);
      return;
    }
  }

  // Recurse into top-level children
  for (const child of node.namedChildren) {
    visitNode(child, filePath, config, entities, parentId, sourceCode);
  }
}

function extractName(node: TreeSitterNode, config: LanguageConfig, sourceCode: string): string | undefined {
  // Try 'name' field first (works for most languages)
  const nameNode = node.childForFieldName('name');
  if (nameNode) {
    return nameNode.text;
  }

  // For variable/lexical declarations, try to get the declarator name
  if (node.type === 'lexical_declaration' || node.type === 'variable_declaration') {
    for (const child of node.namedChildren) {
      if (child.type === 'variable_declarator') {
        const declName = child.childForFieldName('name');
        if (declName) return declName.text;
      }
    }
  }

  // For decorated definitions (Python), look at the inner definition
  if (node.type === 'decorated_definition') {
    for (const child of node.namedChildren) {
      if (child.type === 'function_definition' || child.type === 'class_definition') {
        const innerName = child.childForFieldName('name');
        if (innerName) return innerName.text;
      }
    }
  }

  // Fallback: first identifier child
  for (const child of node.namedChildren) {
    if (child.type === 'identifier' || child.type === 'type_identifier') {
      return child.text;
    }
  }

  return undefined;
}

function mapNodeType(treeSitterType: string): string {
  const mapping: Record<string, string> = {
    function_declaration: 'function',
    function_definition: 'function',
    function_item: 'function',
    method_declaration: 'method',
    method_definition: 'method',
    class_declaration: 'class',
    class_definition: 'class',
    interface_declaration: 'interface',
    type_alias_declaration: 'type',
    type_declaration: 'type',
    type_item: 'type',
    enum_declaration: 'enum',
    enum_item: 'enum',
    struct_item: 'struct',
    impl_item: 'impl',
    trait_item: 'trait',
    mod_item: 'module',
    export_statement: 'export',
    lexical_declaration: 'variable',
    variable_declaration: 'variable',
    var_declaration: 'variable',
    const_declaration: 'constant',
    const_item: 'constant',
    static_item: 'static',
    decorated_definition: 'function',
    public_field_definition: 'property',
    field_definition: 'property',
  };
  return mapping[treeSitterType] ?? treeSitterType;
}
