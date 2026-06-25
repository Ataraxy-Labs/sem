#!/usr/bin/env node
// One-command setup of sem for coding agents: installs the sem skill and
// registers the sem MCP server so the agent uses sem for code intelligence.
//
//   npx @ataraxy-labs/sem-skill
//
// Idempotent: safe to re-run (it overwrites the skill and skips an already
// registered MCP server).

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, copyFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const log = (m) => process.stdout.write(`${m}\n`);

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

log('\nDone. Your agent will now prefer sem (impact / context / orient / diff)');
log('over grep for structural code questions. Restart the agent session to load');
log('the MCP tools.\n');
