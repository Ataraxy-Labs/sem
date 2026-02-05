import { watch } from 'node:fs';
import { resolve, relative } from 'node:path';
import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import { computeSemanticDiff, type DiffResult } from '../../parser/differ.js';
import { formatTerminal } from '../formatters/terminal.js';

export interface WatchOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
  debounce?: number;
}

export async function watchCommand(opts: WatchOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  const repoRoot = await git.getRepoRoot();
  const registry = createDefaultRegistry();
  const debounceMs = opts.debounce ?? 500;

  console.log(chalk.dim(`  Watching ${repoRoot} for semantic changes...`));
  console.log(chalk.dim('  Press Ctrl+C to stop.\n'));

  let timer: ReturnType<typeof setTimeout> | null = null;
  let lastOutput = '';

  const runDiff = async () => {
    try {
      const scope = await git.detectScope();
      const fileChanges = await git.getChangedFiles(scope);

      if (fileChanges.length === 0) {
        if (lastOutput !== 'clean') {
          console.clear();
          console.log(chalk.dim(`  Watching ${repoRoot} for semantic changes...`));
          console.log(chalk.dim('  Press Ctrl+C to stop.\n'));
          console.log(chalk.dim('  No changes detected.'));
          lastOutput = 'clean';
        }
        return;
      }

      const result = computeSemanticDiff(fileChanges, registry);

      if (opts.format === 'json') {
        const output = JSON.stringify({
          timestamp: new Date().toISOString(),
          summary: {
            files: result.fileCount,
            added: result.addedCount,
            modified: result.modifiedCount,
            deleted: result.deletedCount,
            total: result.changes.length,
          },
          changes: result.changes,
        });

        if (output !== lastOutput) {
          console.log(output);
          lastOutput = output;
        }
      } else {
        const output = formatTerminal(result);
        if (output !== lastOutput) {
          console.clear();
          console.log(chalk.dim(`  Watching ${repoRoot} · ${new Date().toLocaleTimeString()}\n`));
          console.log(output);
          lastOutput = output;
        }
      }
    } catch {
      // Ignore transient errors during file writes
    }
  };

  // Initial diff
  await runDiff();

  // Watch for changes
  const watcher = watch(repoRoot, { recursive: true }, (event, filename) => {
    if (!filename) return;
    // Skip .git and .sem directories
    if (filename.startsWith('.git') || filename.startsWith('.sem')) return;
    // Skip node_modules
    if (filename.includes('node_modules')) return;

    if (timer) clearTimeout(timer);
    timer = setTimeout(runDiff, debounceMs);
  });

  // Handle exit
  process.on('SIGINT', () => {
    watcher.close();
    console.log(chalk.dim('\n  Stopped watching.'));
    process.exit(0);
  });
}
