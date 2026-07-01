#!/usr/bin/env node
// One-command setup of sem for coding agents: installs the sem skill and
// registers the sem MCP server so the agent uses sem for code intelligence.
//
//   npx @ataraxy-labs/sem-skill
//
// Idempotent: safe to re-run (it overwrites the skill and skips an already
// registered MCP server).

import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  copyFileSync,
  readFileSync,
  writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const log = (m) => process.stdout.write(`${m}\n`);
const wantBadge = process.argv.slice(2).includes('--badge');

// Opt-in: a live sem activity badge in the Claude Code statusline, fed by a
// PostToolUse hook that logs each sem MCP call. Only runs with --badge, backs
// up settings, and never overwrites a statusline you already have.
function installBadge() {
  const claudeDir = join(homedir(), '.claude');
  const hooksDir = join(claudeDir, 'hooks');
  const slDest = join(claudeDir, 'statusline-sem.py');
  const hookDest = join(hooksDir, 'sem-activity.py');
  try {
    mkdirSync(hooksDir, { recursive: true });
    copyFileSync(join(here, 'badge', 'statusline-sem.py'), slDest);
    copyFileSync(join(here, 'badge', 'sem-activity.py'), hookDest);
    log(`  [ok] installed sem badge scripts -> ${claudeDir}`);
  } catch (e) {
    log(`  [!]  could not install badge scripts: ${e.message}`);
    return;
  }

  const settingsPath = join(claudeDir, 'settings.json');
  let settings = {};
  if (existsSync(settingsPath)) {
    try {
      settings = JSON.parse(readFileSync(settingsPath, 'utf8'));
    } catch {
      settings = {};
    }
    try {
      copyFileSync(settingsPath, `${settingsPath}.bak-${Date.now()}`);
    } catch {}
  }

  // PostToolUse hook: non-destructive, append only if not already present.
  settings.hooks = settings.hooks || {};
  const post = (settings.hooks.PostToolUse = settings.hooks.PostToolUse || []);
  const hasHook = post.some((e) =>
    (e.hooks || []).some((h) => (h.command || '').includes('sem-activity.py')),
  );
  if (!hasHook) {
    // sem is used two ways: the MCP tools and the CLI through Bash. Register
    // both so the badge lights up whichever path the agent takes.
    for (const matcher of ['mcp__sem__.*', 'Bash']) {
      post.push({
        matcher,
        hooks: [{ type: 'command', command: `python3 ${hookDest}` }],
      });
    }
  }

  // statusLine: destructive slot, so only set it if you have none (or it is
  // already ours). Otherwise leave yours alone and print how to add the badge.
  const slCmd = `python3 ${slDest}`;
  const existingSl = settings.statusLine && settings.statusLine.command;
  if (!existingSl || existingSl.includes('statusline-sem.py')) {
    settings.statusLine = { type: 'command', command: slCmd };
    log('  [ok] enabled the live sem statusline badge');
  } else {
    log('  [i]  you already have a statusline; leaving it untouched.');
    log(`       to add the sem badge, set your statusline to: ${slCmd}`);
  }

  try {
    writeFileSync(settingsPath, JSON.stringify(settings, null, 2));
    log('  [ok] updated ~/.claude/settings.json (backup saved)');
  } catch (e) {
    log(`  [!]  could not write settings.json: ${e.message}`);
  }
}

function has(cmd) {
  try {
    execFileSync(process.platform === 'win32' ? 'where' : 'which', [cmd], {
      stdio: 'ignore',
    });
    return true;
  } catch {
    return false;
  }
}

log('\nSetting up sem for your coding agent...\n');

// 1. sem binary check (the skill + MCP both need it).
if (has('sem')) {
  log('  [ok] sem CLI found on PATH');
} else {
  log('  [!]  sem CLI not found on PATH.');
  log('       Install it first:  npm i -g @ataraxy-labs/sem   (or see');
  log('       https://github.com/Ataraxy-Labs/sem#install). Continuing setup;');
  log('       the skill and MCP server will work once sem is installed.');
}

// 2. Install the skill so the agent knows when and how to use sem.
const skillDir = join(homedir(), '.claude', 'skills', 'sem');
try {
  mkdirSync(skillDir, { recursive: true });
  copyFileSync(join(here, 'SKILL.md'), join(skillDir, 'SKILL.md'));
  log(`  [ok] installed sem skill -> ${join(skillDir, 'SKILL.md')}`);
} catch (e) {
  log(`  [!]  could not install skill: ${e.message}`);
}

// 3. Register the sem MCP server (user scope, available in every project).
if (has('claude')) {
  try {
    const existing = execFileSync('claude', ['mcp', 'list'], {
      encoding: 'utf8',
    });
    if (/^sem[:\s]/m.test(existing)) {
      log('  [ok] sem MCP server already registered');
    } else {
      execFileSync('claude', ['mcp', 'add', '-s', 'user', 'sem', '--', 'sem', 'mcp'], {
        stdio: 'ignore',
      });
      log('  [ok] registered sem MCP server (user scope)');
    }
  } catch (e) {
    log(`  [!]  could not register MCP server automatically: ${e.message}`);
    log('       Run manually:  claude mcp add -s user sem -- sem mcp');
  }
} else {
  log('  [i]  claude CLI not found; to enable the MCP tools run:');
  log('       claude mcp add -s user sem -- sem mcp');
}

// 4. Optional: the live sem statusline badge.
if (wantBadge) {
  log('');
  installBadge();
} else {
  log('');
  log('  [i]  optional: a live sem activity badge for your statusline');
  log('       (shows structural queries + latency as you work). Enable with:');
  log('       npx @ataraxy-labs/sem-skill --badge');
}

log('\nDone. Your agent will now prefer sem (impact / context / orient / diff)');
log('over grep for structural code questions. Restart the agent session to load');
log('the MCP tools' + (wantBadge ? ' and show the sem badge' : '') + '.\n');
