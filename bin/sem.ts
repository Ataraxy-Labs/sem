#!/usr/bin/env node

import { Command } from 'commander';
import { initCommand } from '../src/cli/commands/init.js';
import { diffCommand } from '../src/cli/commands/diff.js';
import { logCommand } from '../src/cli/commands/log.js';
import { queryCommand } from '../src/cli/commands/query.js';

const program = new Command();

program
  .name('sem')
  .description('Semantic Version Control — entity-level diffs on top of Git')
  .version('0.1.0');

program
  .command('init')
  .description('Initialize sem in the current Git repository')
  .action(async () => {
    await initCommand();
  });

program
  .command('diff')
  .description('Show semantic diff of changes')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .option('-s, --staged', 'Show staged changes only')
  .option('-c, --commit <sha>', 'Show changes in a specific commit')
  .option('--from <ref>', 'Start of commit range')
  .option('--to <ref>', 'End of commit range')
  .option('--store', 'Store changes in the sem database')
  .action(async (opts) => {
    await diffCommand({
      format: opts.format,
      staged: opts.staged,
      commit: opts.commit,
      from: opts.from,
      to: opts.to,
      store: opts.store,
    });
  });

program
  .command('log')
  .description('Show semantic commit history')
  .option('-n, --count <n>', 'Number of commits to show', '5')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .option('--store', 'Store changes in the sem database')
  .action(async (opts) => {
    await logCommand({
      format: opts.format,
      count: parseInt(opts.count, 10),
      store: opts.store,
    });
  });

program
  .command('query <sql>')
  .description('Run a SQL query against the sem database')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .action(async (sql: string, opts) => {
    await queryCommand(sql, {
      format: opts.format,
    });
  });

program.parse();
