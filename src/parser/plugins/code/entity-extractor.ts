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

type VisitContext = {
  insideFunction: boolean;
};

export function extractEntities(
  tree: TreeSitterTree,
  filePath: string,
  config: LanguageConfig,
  sourceCode: string,
): SemanticEntity[] {
  const entities: SemanticEntity[] = [];
  visitNode(tree.rootNode, filePath, config, entities, undefined, sourceCode, { insideFunction: false });
  return entities;
}

function visitNode(
  node: TreeSitterNode,
  filePath: string,
  config: LanguageConfig,
  entities: SemanticEntity[],
  parentId: string | undefined,
  sourceCode: string,
  context: VisitContext,
): void {
  // For export statements, always look inside for the actual declaration.
  // This avoids losing nested entities when `export` wraps a class/function.
  if (node.type === 'export_statement') {
    const declaration = node.childForFieldName('declaration');
    if (declaration) {
      visitNode(declaration, filePath, config, entities, parentId, sourceCode, context);
      return;
    }
  }

  let currentParentId = parentId;
  if (config.entityNodeTypes.includes(node.type)) {
    const name = extractName(node, config, sourceCode);
    const entityType = mapNodeType(node);
    const shouldSkip =
      (context.insideFunction && entityType === 'variable') ||
      (node.type === 'pair' && !isFunctionLikePair(node));

    if (name && !shouldSkip) {
      const content = node.text;
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
      currentParentId = entity.id;
    }
  }

  const nextContext: VisitContext = {
    insideFunction: context.insideFunction || isFunctionContainer(node),
  };

  // Recurse into children to capture nested entities (e.g. object-literal
  // methods inside factory functions, handlers inside React components).
  for (const child of node.namedChildren) {
    visitNode(child, filePath, config, entities, currentParentId, sourceCode, nextContext);
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

  if (node.type === 'pair') {
    return getPairKeyName(node);
  }

  // Fallback: first identifier child
  for (const child of node.namedChildren) {
    if (child.type === 'identifier' || child.type === 'type_identifier' || child.type === 'property_identifier') {
      return child.text;
    }
  }

  return undefined;
}

function mapNodeType(node: TreeSitterNode): string {
  if (node.type === 'pair') {
    return isFunctionLikePair(node) ? 'method' : 'property';
  }

  if ((node.type === 'lexical_declaration' || node.type === 'variable_declaration') && isFunctionLikeDeclaration(node)) {
    return 'function';
  }

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
  return mapping[node.type] ?? node.type;
}

function isFunctionLikeDeclaration(node: TreeSitterNode): boolean {
  for (const child of node.namedChildren) {
    if (child.type !== 'variable_declarator') continue;

    const value = child.childForFieldName('value');
    if (value && isFunctionLikeNodeType(value.type)) {
      return true;
    }

    const normalized = child.text.replace(/\s+/g, ' ').trim();
    if (/^[^=]+=\s*(?:async\s+)?(?:function\b|\([^)]*\)\s*=>|[A-Za-z_$][\w$]*\s*=>)/.test(normalized)) {
      return true;
    }
  }

  return false;
}

function isFunctionLikeNodeType(nodeType: string): boolean {
  return (
    nodeType === 'arrow_function' ||
    nodeType === 'function' ||
    nodeType === 'function_expression' ||
    nodeType === 'generator_function' ||
    nodeType === 'generator_function_declaration'
  );
}

function isFunctionContainer(node: TreeSitterNode): boolean {
  return (
    node.type === 'function_declaration' ||
    node.type === 'function_definition' ||
    node.type === 'method_definition' ||
    node.type === 'method_declaration' ||
    node.type === 'arrow_function' ||
    node.type === 'function' ||
    node.type === 'function_expression' ||
    node.type === 'generator_function' ||
    node.type === 'generator_function_declaration'
  );
}

function getPairKeyName(node: TreeSitterNode): string | undefined {
  const key = node.childForFieldName('key');
  if (!key) return undefined;
  return key.text.replace(/^['"`]|['"`]$/g, '');
}

function getPairValueNode(node: TreeSitterNode): TreeSitterNode | null {
  return node.childForFieldName('value');
}

function isFunctionLikePair(node: TreeSitterNode): boolean {
  const value = getPairValueNode(node);
  if (!value) return false;
  return isFunctionLikeNodeType(value.type);
}
