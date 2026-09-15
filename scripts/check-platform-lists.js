#!/usr/bin/env node
// Verifies that release.yml's build matrix builds exactly the platforms
// listed in platforms.json (the single source of truth — see that file's
// $comment, and PRODUCTION-READINESS-100.md goals 8-9).
//
// bin/postinstall.js derives its PLATFORM_MAP directly from platforms.json at
// runtime, so it can no longer drift on its own; this script's remaining job
// is to catch release.yml (a separately hand-maintained matrix) diverging
// from platforms.json in either direction — a target released but not
// downloadable, or a target postinstall.js expects that release.yml never
// builds.
//
// Deliberately dependency-free (no YAML parser): release.yml's matrix is
// simple enough that a targeted regex over its known shape is more robust
// here than adding a new dependency this CI check would then also need to
// trust.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = join(__dirname, '..');

const platforms = JSON.parse(readFileSync(join(root, 'platforms.json'), 'utf8')).platforms;
const expectedTargets = new Set(platforms.map((p) => p.rustTarget));

const releaseYml = readFileSync(join(root, '.github', 'workflows', 'release.yml'), 'utf8');

// release.yml's steps use `${{ matrix.target }}` (contains `$`/`{`), which
// this pattern intentionally does not match — only literal matrix.include
// values like `target: x86_64-unknown-linux-gnu` are literal target names.
const targetLinePattern = /^\s+target:\s*([a-z0-9_.-]+)\s*$/gm;
const releasedTargets = new Set();
let match;
while ((match = targetLinePattern.exec(releaseYml)) !== null) {
  releasedTargets.add(match[1]);
}

if (releasedTargets.size === 0) {
  console.error('ERROR: found zero literal `target:` entries in release.yml — regex likely broken, or release.yml build matrix moved/renamed.');
  process.exit(1);
}

const missingFromRelease = [...expectedTargets].filter((t) => !releasedTargets.has(t));
const extraInRelease = [...releasedTargets].filter((t) => !expectedTargets.has(t));

let ok = true;

if (missingFromRelease.length > 0) {
  ok = false;
  console.error('ERROR: platforms.json lists targets that release.yml never builds:');
  for (const t of missingFromRelease) console.error(`  - ${t}`);
}

if (extraInRelease.length > 0) {
  ok = false;
  console.error('ERROR: release.yml builds targets that are missing from platforms.json (postinstall.js will not know how to download them):');
  for (const t of extraInRelease) console.error(`  - ${t}`);
}

if (!ok) {
  console.error('');
  console.error('Update platforms.json and release.yml together so every released target is downloadable, and vice versa.');
  process.exit(1);
}

console.log(`platforms.json and release.yml agree on ${expectedTargets.size} platform targets.`);
