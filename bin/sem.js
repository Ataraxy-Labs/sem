#!/usr/bin/env node

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { getInstalledBinaryPath } from '../scripts/package-meta.mjs';

const binaryPath = getInstalledBinaryPath();

if (!existsSync(binaryPath)) {
  console.error(
    [
      'sem is not installed yet.',
      'Reinstall @ataraxy-labs/sem to download the binary.',
      'If you use Bun, trust the package first so its postinstall script can run.',
    ].join(' '),
  );
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
});

child.on('error', (error) => {
  console.error(`Failed to launch sem: ${error.message}`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }

  process.exit(code ?? 1);
});

for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(signal, () => {
    if (!child.killed) {
      child.kill(signal);
    }
  });
}
