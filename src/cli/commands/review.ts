import chalk from 'chalk';
import { GitBridge } from '../../git/bridge.js';
import { createDefaultRegistry } from '../../parser/plugins/index.js';
import { computeSemanticDiff } from '../../parser/differ.js';
import { formatTerminal } from '../formatters/terminal.js';
import { formatJson } from '../formatters/json.js';

export interface ReviewOptions {
  cwd?: string;
  format?: 'terminal' | 'json';
  base?: string;
}

export async function reviewCommand(branchOrPR: string, opts: ReviewOptions = {}): Promise<void> {
  const cwd = opts.cwd ?? process.cwd();
  const git = new GitBridge(cwd);

  if (!(await git.isRepo())) {
    console.error(chalk.red('Error: Not inside a Git repository.'));
    process.exit(1);
  }

  // Detect if it's a PR number or branch name
  let targetRef = branchOrPR;
  let baseRef = opts.base ?? 'main';
  let prInfo: { title: string; author: string; number: number } | null = null;

  if (/^\d+$/.test(branchOrPR)) {
    // It's a PR number — try to get branch info from gh
    try {
      const { execSync } = await import('node:child_process');
      const prJson = execSync(`gh pr view ${branchOrPR} --json headRefName,baseRefName,title,author`, {
        encoding: 'utf-8',
        cwd,
      });
      const pr = JSON.parse(prJson);
      targetRef = pr.headRefName;
      baseRef = opts.base ?? pr.baseRefName;
      prInfo = { title: pr.title, author: pr.author?.login ?? 'unknown', number: parseInt(branchOrPR) };
    } catch {
      console.error(chalk.red(`Could not fetch PR #${branchOrPR}. Is gh CLI installed?`));
      process.exit(1);
    }
  }

  // Get semantic diff between base and target
  const fileChanges = await git.getChangedFiles({
    type: 'range',
    from: baseRef,
    to: targetRef,
  });

  if (fileChanges.length === 0) {
    console.log(chalk.dim('No changes between branches.'));
    return;
  }

  const registry = createDefaultRegistry();
  const result = computeSemanticDiff(fileChanges, registry);

  if (opts.format === 'json') {
    const output: Record<string, unknown> = {
      base: baseRef,
      target: targetRef,
    };
    if (prInfo) {
      output.pr = prInfo;
    }
    output.summary = {
      files: result.fileCount,
      added: result.addedCount,
      modified: result.modifiedCount,
      deleted: result.deletedCount,
      moved: result.movedCount,
      renamed: result.renamedCount,
      total: result.changes.length,
    };
    output.changes = result.changes;
    console.log(JSON.stringify(output, null, 2));
    return;
  }

  // Terminal output
  if (prInfo) {
    console.log(chalk.bold(`\n  PR #${prInfo.number}: ${prInfo.title}`));
    console.log(chalk.dim(`  by ${prInfo.author} · ${baseRef} ← ${targetRef}\n`));
  } else {
    console.log(chalk.dim(`\n  Review: ${baseRef} ← ${targetRef}\n`));
  }

  console.log(formatTerminal(result));

  // Risk assessment
  const risks: string[] = [];

  const deletedFunctions = result.changes.filter(
    c => c.changeType === 'deleted' && (c.entityType === 'function' || c.entityType === 'method')
  );
  if (deletedFunctions.length > 0) {
    risks.push(chalk.red(`  ⚠  ${deletedFunctions.length} function${deletedFunctions.length > 1 ? 's' : ''} deleted: ${deletedFunctions.map(f => f.entityName).join(', ')}`));
  }

  const modifiedConfigs = result.changes.filter(
    c => c.changeType === 'modified' && (c.entityType === 'property' || c.entityType === 'section')
  );
  if (modifiedConfigs.length > 5) {
    risks.push(chalk.yellow(`  ⚠  ${modifiedConfigs.length} config properties changed — verify production settings`));
  }

  if (result.changes.length > 50) {
    risks.push(chalk.yellow(`  ⚠  Large changeset (${result.changes.length} entities) — consider splitting`));
  }

  if (risks.length > 0) {
    console.log(chalk.bold('\nRisk signals:'));
    for (const risk of risks) {
      console.log(risk);
    }
    console.log('');
  }
}
