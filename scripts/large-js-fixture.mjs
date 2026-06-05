#!/usr/bin/env node

import { execFile } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';

const execFileAsync = promisify(execFile);
const MARKER_FILE = '.sem-large-js-fixture';
const IMPORT_STYLES = new Set(['named', 'default', 'namespace', 'type', 'side-effect']);

export const DEFAULT_LARGE_JS_FIXTURE_OPTIONS = {
  outDir: path.resolve('benchmarks/large-js-fixture/.generated/repo'),
  fileCount: 1000,
  entitiesPerFile: 12,
  importFanout: 5,
  importStyleMix: ['named', 'named', 'named', 'default', 'namespace', 'type'],
  nestedDepth: 3,
  bodyLines: 40,
  language: 'mixed',
  gitInit: true,
  force: false,
};

export function parseLargeJsFixtureArgs(argv) {
  const options = { ...DEFAULT_LARGE_JS_FIXTURE_OPTIONS };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case '-h':
      case '--help':
        return { help: true };
      case '--out':
      case '-o':
        options.outDir = path.resolve(readFlagValue(argv, ++index, arg));
        break;
      case '--files':
      case '--file-count':
        options.fileCount = parsePositiveInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--entities-per-file':
        options.entitiesPerFile = parsePositiveInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--fanout':
      case '--import-fanout':
        options.importFanout = parseNonNegativeInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--import-style-mix':
      case '--import-styles':
        options.importStyleMix = parseImportStyleMix(readFlagValue(argv, ++index, arg));
        break;
      case '--nested-depth':
        options.nestedDepth = parseNonNegativeInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--body-lines':
      case '--large-body-lines':
        options.bodyLines = parseNonNegativeInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--language':
        options.language = readFlagValue(argv, ++index, arg);
        break;
      case '--no-git-init':
        options.gitInit = false;
        break;
      case '--force':
        options.force = true;
        break;
      default:
        throw new Error(`Unknown option "${arg}". Run with --help for usage.`);
    }
  }

  return normalizeLargeJsFixtureOptions(options);
}

export function parseImportStyleMix(value) {
  if (Array.isArray(value)) {
    return value.flatMap((style) => parseImportStyleMixPart(String(style)));
  }

  return String(value)
    .split(',')
    .map((part) => part.trim())
    .filter(Boolean)
    .flatMap(parseImportStyleMixPart);
}

export async function generateLargeJsFixture(options = {}) {
  const normalized = normalizeLargeJsFixtureOptions({
    ...DEFAULT_LARGE_JS_FIXTURE_OPTIONS,
    ...options,
  });

  await prepareOutputDirectory(normalized.outDir, normalized.force);

  const srcDir = path.join(normalized.outDir, 'src');
  await fs.mkdir(srcDir, { recursive: true });

  const fileMetas = [];
  for (let index = 0; index < normalized.fileCount; index += 1) {
    const ext = extensionFor(index, normalized.language);
    const id = fileId(index);
    const relativePath = `src/${id}${ext}`;
    const entityNames = Array.from(
      { length: normalized.entitiesPerFile },
      (_, entityIndex) => `${id}_e${pad(entityIndex, 3)}`,
    );
    const source = renderSourceFile(index, ext, normalized);

    await fs.writeFile(path.join(normalized.outDir, relativePath), source, 'utf8');
    fileMetas.push({
      path: relativePath,
      extension: ext,
      entities: entityNames,
    });
  }

  await writeProjectFiles(normalized.outDir, normalized);

  const entryExt = extensionFor(0, normalized.language);
  const entryFile = `src/${fileId(0)}${entryExt}`;
  const targetEntity = `${fileId(0)}_e000`;
  const markerSearch = incrementalMarkerLine(fileId(0), targetEntity, entryExt, 0);
  const markerReplacement = incrementalMarkerLine(fileId(0), targetEntity, entryExt, 1);
  const manifest = {
    schemaVersion: 1,
    config: {
      fileCount: normalized.fileCount,
      entitiesPerFile: normalized.entitiesPerFile,
      importFanout: normalized.importFanout,
      importStyleMix: normalized.importStyleMix,
      nestedDepth: normalized.nestedDepth,
      bodyLines: normalized.bodyLines,
      language: normalized.language,
    },
    entryFile,
    targetEntity,
    incremental: {
      file: entryFile,
      search: markerSearch,
      replacement: markerReplacement,
    },
    files: fileMetas,
  };

  await fs.writeFile(
    path.join(normalized.outDir, 'fixture-manifest.json'),
    `${stableJson(manifest)}\n`,
    'utf8',
  );

  if (normalized.gitInit) {
    await initializeGitRepository(normalized.outDir);
  }

  return {
    outDir: normalized.outDir,
    manifest,
  };
}

function normalizeLargeJsFixtureOptions(options) {
  const normalized = {
    ...options,
    outDir: path.resolve(String(options.outDir)),
    fileCount: parsePositiveInteger(options.fileCount, 'fileCount'),
    entitiesPerFile: parsePositiveInteger(options.entitiesPerFile, 'entitiesPerFile'),
    importFanout: parseNonNegativeInteger(options.importFanout, 'importFanout'),
    importStyleMix: parseImportStyleMix(options.importStyleMix),
    nestedDepth: parseNonNegativeInteger(options.nestedDepth, 'nestedDepth'),
    bodyLines: parseNonNegativeInteger(options.bodyLines, 'bodyLines'),
    language: String(options.language),
    gitInit: Boolean(options.gitInit),
    force: Boolean(options.force),
  };

  if (!['ts', 'js', 'mixed'].includes(normalized.language)) {
    throw new Error('--language must be ts, js, or mixed.');
  }

  if (normalized.importStyleMix.length === 0) {
    throw new Error('--import-style-mix must contain at least one import style.');
  }

  return normalized;
}

async function prepareOutputDirectory(outDir, force) {
  let entries = [];
  try {
    entries = await fs.readdir(outDir);
  } catch (error) {
    if (error.code !== 'ENOENT') {
      throw error;
    }
  }

  if (entries.length > 0 && !force) {
    try {
      await fs.stat(path.join(outDir, MARKER_FILE));
    } catch {
      throw new Error(`Refusing to replace non-empty directory without ${MARKER_FILE}: ${outDir}`);
    }
  }

  await fs.rm(outDir, { recursive: true, force: true });
  await fs.mkdir(outDir, { recursive: true });
  await fs.writeFile(path.join(outDir, MARKER_FILE), 'generated by scripts/large-js-fixture.mjs\n', 'utf8');
}

async function writeProjectFiles(outDir, options) {
  const packageJson = {
    name: 'sem-large-js-fixture',
    private: true,
    type: 'module',
  };
  const tsconfig = {
    compilerOptions: {
      allowJs: true,
      checkJs: false,
      module: 'ESNext',
      moduleResolution: 'Bundler',
      target: 'ES2022',
      strict: false,
    },
    include: ['src/**/*'],
  };

  await fs.writeFile(path.join(outDir, 'package.json'), `${stableJson(packageJson)}\n`, 'utf8');
  await fs.writeFile(path.join(outDir, 'tsconfig.json'), `${stableJson(tsconfig)}\n`, 'utf8');
  await fs.writeFile(
    path.join(outDir, 'README.md'),
    [
      '# sem Large JS/TS Fixture',
      '',
      'This repository is generated for sem performance testing.',
      `Files: ${options.fileCount}`,
      `Entities per file: ${options.entitiesPerFile}`,
      '',
    ].join('\n'),
    'utf8',
  );
}

function renderSourceFile(index, ext, options) {
  const id = fileId(index);
  const isTs = ext === '.ts';
  const lines = [
    '// Generated fixture source for sem performance testing.',
    ...renderImports(index, ext, options),
  ];

  if (lines[lines.length - 1] !== '') {
    lines.push('');
  }

  if (isTs) {
    lines.push(`export type T${id.slice(1)}_000 = { value: number };`);
    lines.push('');
  }

  lines.push(renderDefaultExport(id, isTs));
  lines.push('');

  for (let entityIndex = 0; entityIndex < options.entitiesPerFile; entityIndex += 1) {
    lines.push(renderEntity(index, entityIndex, ext, options));
    lines.push('');
  }

  lines.push(renderClass(index, ext, options));
  lines.push('');

  return `${lines.join('\n')}`;
}

function renderImports(index, ext, options) {
  const imports = [];
  for (let offset = 1; offset <= options.importFanout; offset += 1) {
    const targetIndex = (index + offset) % options.fileCount;
    if (targetIndex === index) {
      continue;
    }

    const targetExt = extensionFor(targetIndex, options.language);
    const style = effectiveImportStyle(
      options.importStyleMix[(offset - 1) % options.importStyleMix.length],
      ext,
      targetExt,
    );
    const targetId = fileId(targetIndex);
    const importIndex = offset - 1;
    const specifier = `./${targetId}`;

    if (style === 'named') {
      imports.push(
        `import { ${targetId}_e000 as imported_${targetId}_${importIndex} } from '${specifier}';`,
      );
    } else if (style === 'default') {
      imports.push(`import default_${targetId}_${importIndex} from '${specifier}';`);
    } else if (style === 'namespace') {
      imports.push(`import * as ns_${targetId}_${importIndex} from '${specifier}';`);
    } else if (style === 'type') {
      imports.push(`import type { T${targetId.slice(1)}_000 as Type_${targetId}_${importIndex} } from '${specifier}';`);
    } else {
      imports.push(`import '${specifier}';`);
    }
  }

  if (imports.length > 0) {
    imports.push('');
  }

  return imports;
}

function renderDefaultExport(id, isTs) {
  if (isTs) {
    return `export default function default_${id}(input: number): number { return input + ${Number(id.slice(1))}; }`;
  }
  return `export default function default_${id}(input) { return input + ${Number(id.slice(1))}; }`;
}

function renderEntity(index, entityIndex, ext, options) {
  const id = fileId(index);
  const entity = `${id}_e${pad(entityIndex, 3)}`;
  const isTs = ext === '.ts';
  const lines = [];

  lines.push(
    isTs
      ? `export function ${entity}(input: number): number {`
      : `export function ${entity}(input) {`,
  );

  for (let depth = 1; depth <= options.nestedDepth; depth += 1) {
    lines.push(
      isTs
        ? `  function ${entity}_nested_${depth}(value: number): number {`
        : `  function ${entity}_nested_${depth}(value) {`,
    );
    lines.push(`    return value + ${depth};`);
    lines.push('  }');
  }

  if (entityIndex === 0) {
    lines.push(`  ${incrementalMarkerLine(id, entity, ext, 0)}`);
  }

  for (let lineIndex = 0; lineIndex < options.bodyLines; lineIndex += 1) {
    lines.push(`  const local_${entity}_${lineIndex} = input + ${index} + ${entityIndex} + ${lineIndex};`);
  }

  for (const useLine of renderImportUses(index, ext, options)) {
    lines.push(`  ${useLine}`);
  }

  let returnExpression = 'input';
  for (let depth = 1; depth <= options.nestedDepth; depth += 1) {
    returnExpression = `${entity}_nested_${depth}(${returnExpression})`;
  }
  lines.push(`  return ${returnExpression};`);
  lines.push('}');

  return lines.join('\n');
}

function renderClass(index, ext, options) {
  const id = fileId(index);
  const isTs = ext === '.ts';
  const lines = [`export class C${id} {`];
  for (let depth = 0; depth <= options.nestedDepth; depth += 1) {
    lines.push(isTs ? `  m${depth}(input: number): number {` : `  m${depth}(input) {`);
    lines.push(`    return input + ${index} + ${depth};`);
    lines.push('  }');
  }
  lines.push('}');
  return lines.join('\n');
}

function renderImportUses(index, ext, options) {
  const uses = [];
  for (let offset = 1; offset <= options.importFanout; offset += 1) {
    const targetIndex = (index + offset) % options.fileCount;
    if (targetIndex === index) {
      continue;
    }

    const targetExt = extensionFor(targetIndex, options.language);
    const style = effectiveImportStyle(
      options.importStyleMix[(offset - 1) % options.importStyleMix.length],
      ext,
      targetExt,
    );
    const targetId = fileId(targetIndex);
    const importIndex = offset - 1;

    if (style === 'named') {
      uses.push(`const use_named_${targetId}_${importIndex} = imported_${targetId}_${importIndex}(input);`);
    } else if (style === 'default') {
      uses.push(`const use_default_${targetId}_${importIndex} = default_${targetId}_${importIndex}(input);`);
    } else if (style === 'namespace') {
      uses.push(`const use_namespace_${targetId}_${importIndex} = ns_${targetId}_${importIndex}.${targetId}_e000(input);`);
    } else if (style === 'type') {
      uses.push(`const use_type_${targetId}_${importIndex}: Type_${targetId}_${importIndex} = { value: input };`);
    }
  }
  return uses;
}

function effectiveImportStyle(style, sourceExt, targetExt) {
  if (style === 'type' && (sourceExt !== '.ts' || targetExt !== '.ts')) {
    return 'named';
  }
  return style;
}

function incrementalMarkerLine(id, entity, ext, value) {
  if (ext === '.ts') {
    return `const incrementalMarker_${entity}: number = ${value};`;
  }
  return `const incrementalMarker_${entity} = ${value};`;
}

async function initializeGitRepository(outDir) {
  await execFileAsync('git', ['init'], { cwd: outDir });
  await execFileAsync('git', ['config', 'user.email', 'sem-fixture@example.invalid'], { cwd: outDir });
  await execFileAsync('git', ['config', 'user.name', 'sem fixture'], { cwd: outDir });
  await execFileAsync('git', ['add', '.'], { cwd: outDir });
  await execFileAsync('git', ['commit', '-m', 'initial fixture'], { cwd: outDir });
}

function parseImportStyleMixPart(part) {
  const [style, rawWeight = '1'] = part.split(':');
  if (!IMPORT_STYLES.has(style)) {
    throw new Error(`Unknown import style "${style}".`);
  }

  const weight = parsePositiveInteger(rawWeight, `weight for ${style}`);
  return Array.from({ length: weight }, () => style);
}

function extensionFor(index, language) {
  if (language === 'mixed') {
    return index % 2 === 0 ? '.ts' : '.js';
  }
  return language === 'js' ? '.js' : '.ts';
}

function fileId(index) {
  return `f${pad(index, 4)}`;
}

function pad(value, width) {
  return String(value).padStart(width, '0');
}

function stableJson(value) {
  return JSON.stringify(value, null, 2);
}

function readFlagValue(argv, index, flag) {
  const value = argv[index];
  if (value === undefined || value.startsWith('--')) {
    throw new Error(`Missing value for ${flag}.`);
  }
  return value;
}

function parsePositiveInteger(value, label) {
  const text = String(value).trim();
  const parsed = Number.parseInt(text, 10);
  if (!/^\d+$/.test(text) || !Number.isSafeInteger(parsed) || parsed < 1) {
    throw new Error(`${label} must be a positive integer.`);
  }
  return parsed;
}

function parseNonNegativeInteger(value, label) {
  const text = String(value).trim();
  const parsed = Number.parseInt(text, 10);
  if (!/^\d+$/.test(text) || !Number.isSafeInteger(parsed) || parsed < 0) {
    throw new Error(`${label} must be a non-negative integer.`);
  }
  return parsed;
}

const HELP = `
Generate a deterministic JS/TS fixture for sem performance testing.

Usage:
  node scripts/large-js-fixture.mjs --out <dir> [options]

Options:
  --files <count>
  --entities-per-file <count>
  --fanout <count>
  --import-style-mix <mix>
  --nested-depth <count>
  --body-lines <count>
  --language <ts|js|mixed>
  --no-git-init
  --force
`.trim();

async function runCli() {
  const parsed = parseLargeJsFixtureArgs(process.argv.slice(2));
  if (parsed.help) {
    console.log(HELP);
    return;
  }

  const result = await generateLargeJsFixture(parsed);
  console.log(stableJson(result.manifest));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  runCli().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
