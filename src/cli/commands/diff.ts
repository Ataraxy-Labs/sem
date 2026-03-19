import { resolve } from 'node:path';
import { existsSync } from 'node:fs';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import type { DiffScope, FileChange } from '../../git/types.js';
import { ParserRegistry } from '../../parser/registry.js';
import { computeSemanticDiff } from '../../parser/differ.js';
import { SemDatabase } from '../../storage/database.js';
import { formatTerminal } from '../formatters/terminal.js';
import { formatJson } from '../formatters/json.js';
import { formatPlaintext } from '../formatters/plain.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import { loadConfig, validateChanges, formatValidationResults } from './validate.js';

interface ParsedArgs {
  scope?: DiffScope | { type: 'fileCompare'; a: string; b: string };
  pathspecs: string[];
}

function parseGitDiffArgs(args: string[]): ParsedArgs {
  // Split on "--" separator
  const sepIdx = args.indexOf('--');
  let refs: string[];
  let pathspecs: string[];
  if (sepIdx >= 0) {
    refs = args.slice(0, sepIdx);
    pathspecs = args.slice(sepIdx + 1);
  } else {
    refs = args;
    pathspecs = [];
  }

  if (refs.length === 0) {
    return { pathspecs };
  }

  if (refs.length === 1) {
    const arg = refs[0];
    // Check for ... (merge-base) first
    const tripleIdx = arg.indexOf('...');
    if (tripleIdx > 0 && tripleIdx < arg.length - 3) {
      return {
        scope: { type: 'range', from: `${arg.slice(0, tripleIdx)}...mergebase`, to: arg.slice(tripleIdx + 3) },
        pathspecs,
      };
    }
    // Check for .. (range)
    const doubleIdx = arg.indexOf('..');
    if (doubleIdx > 0 && doubleIdx < arg.length - 2) {
      return {
        scope: { type: 'range', from: arg.slice(0, doubleIdx), to: arg.slice(doubleIdx + 2) },
        pathspecs,
      };
    }
    // Single ref
    return { scope: { type: 'refToWorking', refspec: arg }, pathspecs };
  }

  if (refs.length === 2) {
    // Check if both exist as files
    if (pathspecs.length === 0 && existsSync(refs[0]) && existsSync(refs[1])) {
      return { scope: { type: 'fileCompare', a: refs[0], b: refs[1] }, pathspecs };
    }
    return { scope: { type: 'range', from: refs[0], to: refs[1] }, pathspecs };
  }

  console.error(chalk.red('Error: too many positional arguments. Use -- to separate pathspecs.'));
  process.exit(1);
}

export interface DiffOptions {
  cwd?: string;
  format?: 'terminal' | 'json' | 'plain';
  staged?: boolean;
  commit?: string;
  from?: string;
  to?: string;
  store?: boolean;
  args?: string[];
}

// Singleton registry — no need to recreate on every call
let _registry: ParserRegistry | undefined;
function getRegistry(): ParserRegistry {
  return (_registry ??= createDefaultRegistry());
}

export async function diffCommand(opts: DiffOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  const parsed = parseGitDiffArgs(opts.args ?? []);

  let scope: DiffScope;
  let fileChanges: FileChange[];

  // Handle file comparison mode (two file paths)
  if (parsed.scope?.type === 'fileCompare') {
    const { a, b } = parsed.scope;
    // If in a git repo and both resolve as refs, prefer ref comparison
    if (await git.isRepo()) {
      const [aIsRef, bIsRef] = await Promise.all([git.isValidRev(a), git.isValidRev(b)]);
      if (aIsRef && bIsRef) {
        scope = { type: 'range', from: a, to: b };
        fileChanges = await git.getChangedFiles(scope);
        return runDiffPipeline(fileChanges, scope, git, opts);
      }
    }
    // Fall back to direct file comparison
    const { readFile } = await import('node:fs/promises');
    const beforeContent = await readFile(resolve(cwd, a), 'utf-8');
    const afterContent = await readFile(resolve(cwd, b), 'utf-8');
    fileChanges = [{
      filePath: b,
      status: 'modified',
      beforeContent,
      afterContent,
    }];
    scope = { type: 'working' };
    return runDiffPipeline(fileChanges, scope, git, opts);
  }

  // Determine scope from explicit flags, parsed args, or auto-detect
  if (opts.commit) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    scope = { type: 'commit', sha: opts.commit };
    fileChanges = await git.getChangedFiles(scope);
  } else if (opts.from && opts.to) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    scope = { type: 'range', from: opts.from, to: opts.to };
    fileChanges = await git.getChangedFiles(scope);
  } else if (parsed.scope) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    // Handle merge-base range (from contains "...mergebase" marker)
    if (parsed.scope.type === 'range' && parsed.scope.from.endsWith('...mergebase')) {
      const ref1 = parsed.scope.from.slice(0, -'...mergebase'.length);
      const ref2 = parsed.scope.to;
      try {
        const base = await git.resolveMergeBase(ref1, ref2);
        scope = { type: 'range', from: base, to: ref2 };
      } catch (e) {
        console.error(chalk.red(`Error resolving merge base: ${e}`));
        process.exit(1);
      }
    } else {
      scope = parsed.scope as DiffScope;
    }
    fileChanges = await git.getChangedFiles(scope);
  } else if (opts.staged) {
    if (!(await git.isRepo())) { console.error(chalk.red('Error: Not inside a Git repository.')); process.exit(1); }
    scope = { type: 'staged' };
    fileChanges = await git.getChangedFiles(scope);
  } else {
    // Combined: isRepo + detectScope + getChangedFiles in one batch
    try {
      const result = await git.detectAndGetFiles();
      scope = result.scope;
      fileChanges = result.files;
    } catch {
      console.error(chalk.red('Error: Not inside a Git repository.'));
      process.exit(1);
    }
  }

  // Filter by pathspecs if provided
  if (parsed.pathspecs.length > 0) {
    fileChanges = fileChanges.filter(f =>
      parsed.pathspecs.some(spec =>
        f.filePath === spec ||
        f.filePath.startsWith(spec.endsWith('/') ? spec : `${spec}/`)
      )
    );
  }

  return runDiffPipeline(fileChanges, scope!, git, opts);
}

async function runDiffPipeline(
  fileChanges: FileChange[],
  scope: DiffScope,
  git: GitBridge,
  opts: DiffOptions,
): Promise<void> {
  if (fileChanges.length === 0) {
    console.log(chalk.dim('No changes detected.'));
    return;
  }

  // Compute semantic diff
  const registry = getRegistry();
  const commitSha = scope.type === 'commit' ? scope.sha : undefined;
  const result = computeSemanticDiff(fileChanges, registry, commitSha);

  // Get repoRoot once for both store + validation
  let repoRoot: string;
  try {
    repoRoot = await git.getRepoRoot();
  } catch {
    repoRoot = opts.cwd ?? process.cwd();
  }

  // Optionally store changes
  if (opts.store) {
    const dbPath = resolve(repoRoot, '.sem', 'sem.db');
    if (existsSync(dbPath)) {
      const db = new SemDatabase(dbPath);
      db.insertChanges(result.changes);
      db.close();
    }
  }

  // Output
  const format = opts.format ?? 'terminal';
  if (format === 'json') {
    console.log(formatJson(result));
  } else if (format === 'plain') {
    console.log(formatPlaintext(result));
  } else {
    console.log(formatTerminal(result));
  }

  // Run validation rules if .semrc exists
  try {
    const config = await loadConfig(repoRoot);
    if (config.rules && config.rules.length > 0) {
      const violations = validateChanges(result, config);
      if (violations.length > 0) {
        console.log('');
        console.log(formatValidationResults(violations));
      }
    }
  } catch {
    // No config or invalid config — skip validation
  }
}
