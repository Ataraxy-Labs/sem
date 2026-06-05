#!/usr/bin/env node

import fs from 'node:fs/promises';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';
import {
  DEFAULT_LARGE_JS_FIXTURE_OPTIONS,
  generateLargeJsFixture,
  parseImportStyleMix,
} from '../../scripts/large-js-fixture.mjs';

const BENCHMARK_DIR = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_OUT_DIR = path.join(BENCHMARK_DIR, '.generated', 'repo');
const DEFAULT_CACHE_DIR = path.join(BENCHMARK_DIR, '.generated', 'cache');
const DEFAULT_RESULTS_PATH = path.join(BENCHMARK_DIR, '.generated', 'results.json');
const COMMANDS = new Set(['graph', 'impact', 'context', 'verify']);
const IMPACT_MODES = new Set(['deps', 'dependents', 'tests', 'all']);

export const LARGE_JS_FIXTURE_BENCHMARK_HELP = `
Run cold, warm, and one-file incremental sem timings on a generated JS/TS fixture.

Usage:
  node benchmarks/large-js-fixture/run.mjs [options]

Fixture knobs:
  --out <dir>                     Fixture root directory.
  --files <count>                 Number of source files.
  --entities-per-file <count>     Top-level value entities per file.
  --fanout <count>                Imported neighbor files per source file.
  --import-style-mix <mix>        Comma list or weights: named:3,default,namespace,type.
  --nested-depth <count>          Nested local function depth inside each entity.
  --body-lines <count>            Extra executable lines in each entity body.
  --language <ts|js|mixed>        Source extension mix.

Benchmark knobs:
  --sem <path>                    sem binary to execute. Defaults to SEM_BIN or sem.
  --cache-dir <dir>               Cache root outside the fixture root.
  --command <graph|impact|context|verify>
  --impact-mode <deps|dependents|tests|all>
  --target-entity <name>          Entity for impact/context.
  --target-file <path>            Fixture-relative file for impact/context.
  --context-budget <tokens>       Token budget for context command.
  --timeout-ms <millis>           Per-run timeout.
  --json-out <path>               Write machine-readable results.
  --no-json-out                   Do not write a results file.
  --no-git-init                   Do not initialize the fixture as its own Git repo.
  --force                         Allow deleting a non-empty output directory without marker.
  -h, --help                      Show this help.

Example:
  node benchmarks/large-js-fixture/run.mjs --files 1000 --entities-per-file 12 --fanout 5 --body-lines 40
`.trim();

export function parseLargeJsFixtureBenchmarkArgs(argv, env = process.env) {
  const options = {
    sem: env.SEM_BIN || 'sem',
    outDir: DEFAULT_OUT_DIR,
    cacheDir: DEFAULT_CACHE_DIR,
    command: 'graph',
    impactMode: 'deps',
    contextBudget: 8000,
    timeoutMs: 120000,
    jsonOut: DEFAULT_RESULTS_PATH,
    fileCount: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.fileCount,
    entitiesPerFile: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.entitiesPerFile,
    importFanout: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.importFanout,
    importStyleMix: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.importStyleMix,
    nestedDepth: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.nestedDepth,
    bodyLines: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.bodyLines,
    language: DEFAULT_LARGE_JS_FIXTURE_OPTIONS.language,
    gitInit: true,
    force: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];

    switch (arg) {
      case '-h':
      case '--help':
        return { help: true };
      case '--sem':
        options.sem = readFlagValue(argv, ++index, arg);
        break;
      case '--out':
      case '-o':
        options.outDir = readFlagValue(argv, ++index, arg);
        break;
      case '--cache-dir':
        options.cacheDir = readFlagValue(argv, ++index, arg);
        break;
      case '--command':
        options.command = readFlagValue(argv, ++index, arg);
        break;
      case '--impact-mode':
        options.impactMode = readFlagValue(argv, ++index, arg);
        break;
      case '--target-entity':
        options.targetEntity = readFlagValue(argv, ++index, arg);
        break;
      case '--target-file':
        options.targetFile = readFlagValue(argv, ++index, arg);
        break;
      case '--context-budget':
        options.contextBudget = parsePositiveInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--timeout-ms':
        options.timeoutMs = parsePositiveInteger(readFlagValue(argv, ++index, arg), arg);
        break;
      case '--json-out':
        options.jsonOut = readFlagValue(argv, ++index, arg);
        break;
      case '--no-json-out':
        options.jsonOut = null;
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
      case '--import-styles':
      case '--import-style-mix':
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

  return normalizeLargeJsFixtureBenchmarkOptions(options);
}

export function normalizeLargeJsFixtureBenchmarkOptions(options) {
  const normalized = {
    ...options,
    sem: normalizeSemExecutable(options.sem),
    outDir: path.resolve(String(options.outDir)),
    cacheDir: path.resolve(String(options.cacheDir)),
    command: String(options.command),
    impactMode: String(options.impactMode),
    contextBudget: parsePositiveInteger(options.contextBudget, 'contextBudget'),
    timeoutMs: parsePositiveInteger(options.timeoutMs, 'timeoutMs'),
    jsonOut: options.jsonOut === null ? null : path.resolve(String(options.jsonOut)),
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

  if (!COMMANDS.has(normalized.command)) {
    throw new Error(`Unsupported command "${normalized.command}". Use graph, impact, context, or verify.`);
  }
  if (!IMPACT_MODES.has(normalized.impactMode)) {
    throw new Error(`Unsupported --impact-mode "${normalized.impactMode}". Use deps, dependents, tests, or all.`);
  }

  assertCacheOutsideFixture(normalized.cacheDir, normalized.outDir);
  return normalized;
}

function normalizeSemExecutable(value) {
  const executable = String(value);
  if (
    executable.startsWith('.') ||
    executable.includes('/') ||
    executable.includes(path.sep)
  ) {
    return path.resolve(executable);
  }
  return executable;
}

export async function runLargeJsFixtureBenchmark(options) {
  const normalized = normalizeLargeJsFixtureBenchmarkOptions(options);
  const fixture = await generateLargeJsFixture({
    outDir: normalized.outDir,
    fileCount: normalized.fileCount,
    entitiesPerFile: normalized.entitiesPerFile,
    importFanout: normalized.importFanout,
    importStyleMix: normalized.importStyleMix,
    nestedDepth: normalized.nestedDepth,
    bodyLines: normalized.bodyLines,
    language: normalized.language,
    gitInit: normalized.gitInit,
    force: normalized.force,
  });

  await fs.rm(normalized.cacheDir, { recursive: true, force: true });
  await fs.mkdir(normalized.cacheDir, { recursive: true });

  const command = buildSemCommand(normalized, fixture.manifest);
  const env = {
    ...process.env,
    SEM_CACHE_DIR: normalized.cacheDir,
  };

  const runs = [];
  runs.push(await runSemTimed('cold-cache', normalized.sem, command.args, fixture.outDir, env, normalized.timeoutMs));
  runs.push(await runSemTimed('warm-cache', normalized.sem, command.args, fixture.outDir, env, normalized.timeoutMs));

  await applyIncrementalMutation(fixture.outDir, fixture.manifest.incremental);
  runs.push(
    await runSemTimed(
      'incremental-one-file',
      normalized.sem,
      command.args,
      fixture.outDir,
      env,
      normalized.timeoutMs,
    ),
  );

  const result = {
    schemaVersion: 1,
    fixture: {
      outDir: fixture.outDir,
      cacheDir: normalized.cacheDir,
      files: fixture.manifest.files.length,
      config: fixture.manifest.config,
      incremental: fixture.manifest.incremental,
    },
    command,
    timings: Object.fromEntries(runs.map((run) => [run.phase, run.elapsedMs])),
    runs,
  };

  if (normalized.jsonOut) {
    await fs.mkdir(path.dirname(normalized.jsonOut), { recursive: true });
    await fs.writeFile(normalized.jsonOut, `${JSON.stringify(result, null, 2)}\n`, 'utf8');
    result.resultsPath = normalized.jsonOut;
  }

  return result;
}

export function buildSemCommand(options, manifest) {
  const targetEntity = options.targetEntity || manifest.targetEntity;
  const targetFile = options.targetFile || manifest.entryFile;
  const common = ['--json', '--file-exts', '.ts', '.js'];

  if (options.command === 'graph') {
    return {
      kind: 'graph',
      args: ['graph', '.', ...common],
    };
  }

  if (options.command === 'verify') {
    return {
      kind: 'verify',
      args: ['verify', ...common],
    };
  }

  if (options.command === 'impact') {
    const modeFlag = {
      deps: '--deps',
      dependents: '--dependents',
      tests: '--tests',
      all: null,
    }[options.impactMode || 'deps'];
    const args = ['impact', targetEntity, '--file', targetFile];
    if (modeFlag) {
      args.push(modeFlag);
    }
    args.push(...common);
    return {
      kind: 'impact',
      impactMode: options.impactMode || 'deps',
      targetEntity,
      targetFile,
      args,
    };
  }

  return {
    kind: 'context',
    targetEntity,
    targetFile,
    args: [
      'context',
      targetEntity,
      '--file',
      targetFile,
      '--budget',
      String(options.contextBudget),
      ...common,
    ],
  };
}

async function runSemTimed(phase, sem, args, cwd, env, timeoutMs) {
  const startedAt = performance.now();
  let stderr = '';
  let timedOut = false;

  return await new Promise((resolve, reject) => {
    const child = spawn(sem, args, {
      cwd,
      env,
      stdio: ['ignore', 'ignore', 'pipe'],
    });

    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill('SIGTERM');
      setTimeout(() => child.kill('SIGKILL'), 1000).unref();
    }, timeoutMs);

    child.stderr.on('data', (chunk) => {
      if (stderr.length < 65536) {
        stderr += chunk.toString('utf8');
      }
    });

    child.on('error', (error) => {
      clearTimeout(timeout);
      reject(error);
    });

    child.on('close', (code, signal) => {
      clearTimeout(timeout);
      const elapsedMs = Number((performance.now() - startedAt).toFixed(3));

      if (timedOut) {
        reject(new Error(`${phase} timed out after ${timeoutMs} ms: ${sem} ${args.join(' ')}`));
        return;
      }

      if (code !== 0) {
        reject(
          new Error(
            `${phase} failed with exit ${code ?? signal}: ${sem} ${args.join(' ')}\n${stderr}`,
          ),
        );
        return;
      }

      resolve({
        phase,
        elapsedMs,
      });
    });
  });
}

async function applyIncrementalMutation(outDir, mutation) {
  const target = path.join(outDir, mutation.file);
  const source = await fs.readFile(target, 'utf8');
  if (!source.includes(mutation.search)) {
    throw new Error(`Incremental marker not found in ${mutation.file}: ${mutation.search}`);
  }

  await fs.writeFile(target, source.replace(mutation.search, mutation.replacement), 'utf8');
  const timestamp = new Date(Date.now() + 1500);
  await fs.utimes(target, timestamp, timestamp);
}

function assertCacheOutsideFixture(cacheDir, outDir) {
  const relative = path.relative(outDir, cacheDir);
  if (relative === '' || (!relative.startsWith('..') && !path.isAbsolute(relative))) {
    throw new Error(`--cache-dir must be outside --out so sem accepts it: ${cacheDir}`);
  }
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

function printSummary(result) {
  console.log('Large JS/TS fixture benchmark');
  console.log(`fixture: ${result.fixture.outDir}`);
  console.log(`cache: ${result.fixture.cacheDir}`);
  console.log(`command: ${result.command.args.join(' ')}`);
  for (const run of result.runs) {
    console.log(`${run.phase}: ${run.elapsedMs.toFixed(3)} ms`);
  }
  if (result.resultsPath) {
    console.log(`results: ${result.resultsPath}`);
  }
}

async function runCli() {
  const parsed = parseLargeJsFixtureBenchmarkArgs(process.argv.slice(2));
  if (parsed.help) {
    console.log(LARGE_JS_FIXTURE_BENCHMARK_HELP);
    return;
  }

  const result = await runLargeJsFixtureBenchmark(parsed);
  printSummary(result);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  runCli().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
