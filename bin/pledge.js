#!/usr/bin/env node
// Pledgepack CLI launcher — resolves the native binary for the current platform
// and forwards all arguments to it.
//
// Resolution order:
//   1. Local cargo build (target/release or target/debug — dev mode)
//   2. Postinstall download location (bin/{platform}/{binary})
//   3. Direct binary in bin/ (legacy)

import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { existsSync } from 'node:fs';
import { platform, arch } from 'node:os';

const __dirname = dirname(fileURLToPath(import.meta.url));

const plat = platform();
const ar = arch();
const binaryName = plat === 'win32' ? 'pledge.exe' : 'pledge';
const platformKey = `${plat}-${ar}`;

// Resolve binary: local build → postinstall download → direct
let binaryPath = null;

const candidates = [
  join(__dirname, '..', 'target', 'release', binaryName),
  join(__dirname, '..', 'target', 'debug', binaryName),
  join(__dirname, platformKey, binaryName),
  join(__dirname, 'platform', platformKey, binaryName),
  join(__dirname, binaryName),
];

for (const candidate of candidates) {
  if (existsSync(candidate)) {
    binaryPath = candidate;
    break;
  }
}

if (!binaryPath) {
  console.error('');
  console.error('  \x1b[31mpledge\x1b[0m binary not found.');
  console.error('');
  console.error('  Platform: ' + platformKey);
  console.error('');
  console.error('  This can happen if:');
  console.error('    1. The postinstall script failed to download the binary');
  console.error('    2. Your platform is not yet supported');
  console.error('    3. You installed with --ignore-scripts');
  console.error('');
  console.error('  To fix:');
  console.error('    npm rebuild pledgepack');
  console.error('');
  console.error('  Or build from source:');
  console.error('    git clone https://github.com/pledgeandgrow/pledgepack');
  console.error('    cd pledgepack && cargo build --release');
  console.error('');
  process.exit(1);
}

// Forward all arguments to the native binary
const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  cwd: process.cwd(),
});

// Forward termination signals to the native binary. Without this, `kill <pid>`
// / `docker stop` / an npm-script teardown ends only this wrapper and leaves
// the dev server running as an orphan. (SIGINT is not forwarded: a terminal
// Ctrl+C is already delivered to the whole foreground process group.)
for (const sig of ['SIGTERM', 'SIGHUP']) {
  process.on(sig, () => {
    if (!child.killed) child.kill(sig);
  });
}

child.on('exit', (code, signal) => {
  if (signal) {
    // Mirror death-by-signal so callers see the real cause (exit status
    // 128+n on POSIX shells) rather than a generic 1.
    process.removeAllListeners(signal);
    try {
      process.kill(process.pid, signal);
      return;
    } catch {
      /* fall through to a plain exit code */
    }
  }
  process.exit(code ?? 1);
});

child.on('error', (err) => {
  console.error('');
  console.error('  \x1b[31mpledge\x1b[0m: Failed to launch binary: ' + err.message);
  console.error('');
  process.exit(1);
});
