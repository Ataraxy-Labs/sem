#!/usr/bin/env node

import { Command } from 'commander';
import { initCommand } from '../src/cli/commands/init.js';
import { diffCommand } from '../src/cli/commands/diff.js';
import { logCommand } from '../src/cli/commands/log.js';
import { queryCommand } from '../src/cli/commands/query.js';
import { blameCommand } from '../src/cli/commands/blame.js';
import { statusCommand } from '../src/cli/commands/status.js';
import { watchCommand } from '../src/cli/commands/watch.js';
import { reviewCommand } from '../src/cli/commands/review.js';
import { historyCommand } from '../src/cli/commands/history.js';
import { labelCommand } from '../src/cli/commands/label.js';
import { commentCommand } from '../src/cli/commands/comment.js';

const program = new Command();

program
  .name('sem')
  .description('Semantic Version Control — entity-level diffs on top of Git')
  .version('0.2.0');

program
  .command('init')
  .description('Initialize sem in the current Git repository')
  .action(async () => {
    await initCommand();
  });

program
  .command('status')
  .description('Show semantic status of changes')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .action(async (opts) => {
    await statusCommand({ format: opts.format });
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
  .command('blame <file>')
  .description('Show who last touched each entity in a file')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .option('-d, --depth <n>', 'Number of commits to search', '50')
  .action(async (file: string, opts) => {
    await blameCommand(file, {
      format: opts.format,
      depth: parseInt(opts.depth, 10),
    });
  });

program
  .command('history <entity>')
  .description('Show full history of an entity (e.g. "auth.ts::function::login")')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .option('-d, --depth <n>', 'Number of commits to search', '50')
  .action(async (entity: string, opts) => {
    await historyCommand(entity, {
      format: opts.format,
      depth: parseInt(opts.depth, 10),
    });
  });

program
  .command('review <branch-or-pr>')
  .description('Semantic review of a branch or PR (e.g. "feature-branch" or "42")')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .option('-b, --base <ref>', 'Base branch to compare against', 'main')
  .action(async (target: string, opts) => {
    await reviewCommand(target, {
      format: opts.format,
      base: opts.base,
    });
  });

program
  .command('watch')
  .description('Watch for semantic changes in real-time')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .option('--debounce <ms>', 'Debounce interval in ms', '500')
  .action(async (opts) => {
    await watchCommand({
      format: opts.format,
      debounce: parseInt(opts.debounce, 10),
    });
  });

program
  .command('query <sql>')
  .description('Run a SQL query against the sem database')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .action(async (sql: string, opts) => {
    await queryCommand(sql, { format: opts.format });
  });

program
  .command('label <entity-id> [label]')
  .description('Add, remove, or list labels on an entity')
  .option('-r, --remove', 'Remove the label')
  .option('-l, --list', 'List labels')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .action(async (entityId: string, label: string | undefined, opts) => {
    await labelCommand(entityId, label, {
      remove: opts.remove,
      list: opts.list,
      format: opts.format,
    });
  });

program
  .command('comment <entity-id> [body]')
  .description('Add or view comments on an entity')
  .option('-a, --author <name>', 'Comment author')
  .option('--reply <id>', 'Reply to comment ID')
  .option('-f, --format <format>', 'Output format: terminal or json', 'terminal')
  .action(async (entityId: string, body: string | undefined, opts) => {
    await commentCommand(entityId, body, {
      author: opts.author,
      reply: opts.reply ? parseInt(opts.reply, 10) : undefined,
      format: opts.format,
    });
  });

program.parse();
