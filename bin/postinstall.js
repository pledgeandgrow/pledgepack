// Pledgepack postinstall script — downloads the native binary for the current platform.
// In development, the binary is already built via cargo and this is a no-op.

import { existsSync, mkdirSync, chmodSync, rmSync, writeFileSync, readFileSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { platform, arch } from 'node:os';
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Read the package version to download the matching binary release
const pkgJson = JSON.parse(readFileSync(join(__dirname, '..', 'package.json'), 'utf8'));
const PKG_VERSION = pkgJson.version;

// Platform mapping — maps Node.js platform/arch to our release targets.
// Derived from platforms.json (the single source of truth shared with
// release.yml's build matrix and scripts/check-platform-lists.js) instead of
// being hand-maintained here, so this list can no longer silently drift out
// of sync with what release.yml actually publishes — see
// PRODUCTION-READINESS-100.md goals 8-9. This previously omitted
// win32-arm64 even though release.yml built and published it.
const platformsJson = JSON.parse(readFileSync(join(__dirname, '..', 'platforms.json'), 'utf8'));
const PLATFORM_MAP = Object.fromEntries(
  platformsJson.platforms.map((p) => [
    `${p.npmPlatform}-${p.npmArch}`,
    { target: p.rustTarget, ext: p.archive === 'zip' ? '.zip' : '.tar.gz' },
  ]),
);

const platformKey = `${platform()}-${arch()}`;
const mapped = PLATFORM_MAP[platformKey];
const binaryName = platform() === 'win32' ? 'pledge.exe' : 'pledge';

// 1. Check if binary already exists in target/ (dev mode — already built)
const localRelease = join(__dirname, '..', 'target', 'release', binaryName);
const localDebug = join(__dirname, '..', 'target', 'debug', binaryName);

if (existsSync(localRelease) || existsSync(localDebug)) {
  process.exit(0);
}

// 2. Check if binary already downloaded by a previous run
const stagedBinary = join(__dirname, platformKey, binaryName);
if (existsSync(stagedBinary)) {
  process.exit(0);
}

// 3. Check if binary is directly in bin/ (already installed)
const directBinary = join(__dirname, binaryName);
if (existsSync(directBinary)) {
  process.exit(0);
}

if (!mapped) {
  console.warn('');
  console.warn('  \x1b[33mpledge\x1b[0m: No prebuilt binary for ' + platformKey);
  console.warn('  Build from source: cargo build --release');
  console.warn('');
  process.exit(0);
}

// Explicit opt-out (offline installs, CI images that provide the binary
// another way): skip the download entirely, exit 0.
if (process.env.PLEDGE_SKIP_DOWNLOAD === '1') {
  console.warn('  \x1b[33mpledge\x1b[0m: PLEDGE_SKIP_DOWNLOAD=1 — not downloading the native binary.');
  process.exit(0);
}

// The published Linux binaries are linked against glibc. On musl-based
// distributions (Alpine, ...) they cannot start ("not found" from a missing
// dynamic loader), so refuse to install a binary that is known not to run and
// say why, instead of "succeeding" and failing at first launch.
function isMusl() {
  if (platform() !== 'linux') return false;
  try {
    if (process.report) {
      process.report.excludeNetwork = true;
      const header = process.report.getReport().header;
      if (header) return !header.glibcVersionRuntime;
    }
  } catch {
    /* fall through to the ldd probe */
  }
  try {
    return readFileSync('/usr/bin/ldd', 'utf8').includes('musl');
  } catch {
    return false;
  }
}

if (isMusl()) {
  console.warn('');
  console.warn('  \x1b[33mpledge\x1b[0m: musl libc detected (' + platformKey + ') — the prebuilt Linux binaries target glibc.');
  console.warn('  Use a glibc-based image (e.g. Debian/Ubuntu) or build from source: cargo build --release');
  console.warn('');
  process.exit(0);
}

// Download the prebuilt binary from GitHub Releases (version-specific).
// PLEDGE_DOWNLOAD_BASE points at a mirror that lays files out the same way
// (`<base>/<archive>`, `<base>/<archive>.sha256`, ...).
const GITHUB_REPO = 'pledgeandgrow/pledgepack';
const RELEASE_TAG = `v${PKG_VERSION}`;
const packageName = `pledge-${mapped.target}${mapped.ext}`;
const downloadBase = (
  process.env.PLEDGE_DOWNLOAD_BASE ||
  `https://github.com/${GITHUB_REPO}/releases/download/${RELEASE_TAG}`
).replace(/\/+$/, '');
const downloadUrl = `${downloadBase}/${packageName}`;
const DOWNLOAD_TIMEOUT_MS = 120_000;

// Integrity failures (checksum missing/mismatch, bad signature) are fatal:
// they must not fall through to "try something else and exit 0".
class IntegrityError extends Error {}
class HttpError extends Error {}

// Proxy configured for this install (npm exposes its own proxy settings to
// lifecycle scripts as npm_config_*; plain env vars are the usual fallback).
function configuredProxy() {
  const env = process.env;
  return (
    env.npm_config_https_proxy ||
    env.npm_config_proxy ||
    env.HTTPS_PROXY ||
    env.https_proxy ||
    env.HTTP_PROXY ||
    env.http_proxy ||
    ''
  );
}

// Node's built-in fetch ignores HTTP(S)_PROXY, so behind a proxy (or when fetch
// fails for any other network reason) retry once through curl, which honours
// the proxy configuration. Returns a Buffer or null.
function curlGet(url) {
  const args = ['-fsSL', '--proto', '=https,http', '--max-time', String(DOWNLOAD_TIMEOUT_MS / 1000)];
  const proxy = configuredProxy();
  if (proxy) args.push('--proxy', proxy);
  args.push(url);
  const r = spawnSync('curl', args, { stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 512 * 1024 * 1024 });
  return r.status === 0 && r.stdout ? r.stdout : null;
}

async function httpGet(url) {
  try {
    const res = await fetch(url, {
      redirect: 'follow',
      signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS),
    });
    if (!res.ok) {
      throw new HttpError(`HTTP ${res.status} — ${res.statusText}`);
    }
    return Buffer.from(await res.arrayBuffer());
  } catch (err) {
    // A real HTTP answer (404, 403...) is final; only transport failures retry.
    if (err instanceof HttpError) throw err;
    const viaCurl = curlGet(url);
    if (viaCurl) return viaCurl;
    throw err;
  }
}

const destDir = join(__dirname, platformKey);
const archivePath = join(destDir, packageName);

// Verify a downloaded buffer against an expected SHA256 checksum.
//
// Hard-fails by default if no checksum could be fetched at all — previously
// this only warned and installed the unverified binary anyway, which made
// the checksum check a no-op for anyone whose network dropped the sidecar
// request (see PRODUCTION-READINESS-100.md goal 26). Set
// PLEDGE_SKIP_CHECKSUM=1 to explicitly opt out (e.g. an internal mirror that
// doesn't publish .sha256 sidecars) instead of silently degrading.
async function verifyChecksum(buffer, expectedChecksum) {
  if (!expectedChecksum) {
    if (process.env.PLEDGE_SKIP_CHECKSUM === '1') {
      console.warn('  \x1b[33mpledge\x1b[0m: No checksum available — skipping verification (PLEDGE_SKIP_CHECKSUM=1).');
      return;
    }
    throw new IntegrityError(
      'No checksum available for this binary — refusing to install unverified. ' +
        'Set PLEDGE_SKIP_CHECKSUM=1 to install anyway.',
    );
  }
  if (!/^[0-9a-fA-F]{64}$/.test(expectedChecksum)) {
    // e.g. an HTML error page served with a 200 by a proxy/captive portal
    throw new IntegrityError('Checksum file is not a valid SHA-256 digest — refusing to install.');
  }
  const actual = crypto.createHash('sha256').update(buffer).digest();
  const expected = Buffer.from(expectedChecksum, 'hex');
  if (!crypto.timingSafeEqual(actual, expected)) {
    throw new IntegrityError(
      `Checksum mismatch: expected ${expectedChecksum.toLowerCase()}, got ${actual.toString('hex')}`,
    );
  }
}

// Best-effort Sigstore/cosign verification of the downloaded binary against
// the keyless signature release.yml publishes alongside it (`.sig` +
// `.pem`). Unlike the checksum above, this is opportunistic rather than a
// hard requirement: cosign is a niche CLI most npm install environments
// won't have, so mandating it would break installs for the common case. If
// it IS present, a verification failure is fatal — a forged signature is a
// worse outcome than "cosign not installed". If it's absent, this warns and
// proceeds (the checksum above still protects against corruption/tampering
// in transit; it just doesn't prove provenance the way cosign does). See
// PRODUCTION-READINESS-100.md goal 25 — the cosign signing step in
// release.yml previously had no consumer at all.
function escapeRegex(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

async function verifyCosignSignature(archivePath, downloadUrl) {
  const cosignCheck = spawnSync('cosign', ['version'], { stdio: 'pipe' });
  if (cosignCheck.status !== 0) {
    console.warn(
      '  \x1b[33mpledge\x1b[0m: cosign not found — skipping signature verification (checksum above still applies).',
    );
    console.warn('  Install cosign for full supply-chain verification: https://docs.sigstore.dev/cosign/system_config/installation/');
    return;
  }

  const sigPath = archivePath + '.sig';
  const certPath = archivePath + '.pem';
  let sigOk = true;
  for (const [url, dest] of [
    [downloadUrl + '.sig', sigPath],
    [downloadUrl + '.pem', certPath],
  ]) {
    try {
      writeFileSync(dest, await httpGet(url));
    } catch {
      sigOk = false;
      break;
    }
  }

  if (!sigOk) {
    console.warn(
      '  \x1b[33mpledge\x1b[0m: cosign is installed but no .sig/.pem was published for this release — skipping signature verification.',
    );
    return;
  }

  const verify = spawnSync(
    'cosign',
    [
      'verify-blob',
      '--signature', sigPath,
      '--certificate', certPath,
      // Pin the signer to this repository's release workflow. A `.*`
      // identity accepts a valid signature from ANY GitHub workflow, so an
      // attacker could sign a forged binary with their own identity and
      // still pass verification.
      '--certificate-identity-regexp',
      `^https://github\\.com/${escapeRegex(GITHUB_REPO)}/\\.github/workflows/release\\.yml@refs/(tags|heads)/.+$`,
      '--certificate-oidc-issuer', 'https://token.actions.githubusercontent.com',
      archivePath,
    ],
    { stdio: 'pipe' },
  );

  try { rmSync(sigPath, { force: true }); rmSync(certPath, { force: true }); } catch {}

  if (verify.status !== 0) {
    throw new IntegrityError(
      'cosign signature verification FAILED for the downloaded binary — refusing to install. ' +
        (verify.stderr ? verify.stderr.toString() : ''),
    );
  }

  console.log('  \x1b[32m✓\x1b[0m cosign signature verified.');
}

async function downloadAndExtract() {
  console.log('');
  console.log('  \x1b[36mpledge\x1b[0m: Downloading native binary...');
  console.log('  \x1b[2m  → ' + downloadUrl + '\x1b[0m');
  console.log('');

  mkdirSync(destDir, { recursive: true });

  // Download the archive
  try {
    const buffer = await httpGet(downloadUrl);

    // Verify checksum (the .sha256 sidecar published alongside the release)
    const checksumUrl = downloadUrl + '.sha256';
    let expectedChecksum = null;
    try {
      expectedChecksum = (await httpGet(checksumUrl)).toString('utf8').trim().split(/\s+/)[0];
    } catch (e) {
      console.warn('  \x1b[33mpledge\x1b[0m: Could not fetch checksum: ' + e.message);
    }

    await verifyChecksum(buffer, expectedChecksum);
    if (expectedChecksum) {
      console.log('  \x1b[32m✓\x1b[0m Checksum verified.');
    }

    writeFileSync(archivePath, buffer);
    await verifyCosignSignature(archivePath, downloadUrl);
  } catch (err) {
    if (err instanceof IntegrityError) {
      // Never "fall back" past a failed integrity check: fail the install.
      console.error('');
      console.error('  \x1b[31mpledge\x1b[0m: ' + err.message);
      console.error('');
      rmSync(archivePath, { force: true });
      process.exit(1);
    }
    console.warn('');
    console.warn('  \x1b[33mpledge\x1b[0m: Failed to download binary: ' + err.message);
    console.warn('  Falling back to source build...');
    console.warn('');
    return tryBuildFromSource();
  }

  // Extract the archive
  if (mapped.ext === '.zip') {
    // Windows: use PowerShell to extract
    const result = spawnSync('powershell', [
      '-NoProfile', '-Command',
      `Expand-Archive -LiteralPath '${archivePath.replace(/'/g, "''")}' -DestinationPath '${destDir.replace(/'/g, "''")}' -Force`
    ], { stdio: 'inherit' });
    if (result.status !== 0) {
      // Windows 10+ ships bsdtar, which extracts zip archives too; use it if
      // PowerShell is missing/restricted (e.g. Constrained Language Mode).
      const viaTar = spawnSync('tar', ['-xf', archivePath, '-C', destDir], { stdio: 'inherit' });
      if (viaTar.status !== 0) {
        console.warn('  \x1b[31mpledge\x1b[0m: Failed to extract zip');
        return tryBuildFromSource();
      }
    }
  } else {
    // Unix: use tar
    const result = spawnSync('tar', ['xzf', archivePath, '-C', destDir], { stdio: 'inherit' });
    if (result.status !== 0) {
      console.warn('  \x1b[31mpledge\x1b[0m: Failed to extract tar.gz');
      return tryBuildFromSource();
    }
  }

  // Clean up the archive
  try { rmSync(archivePath, { force: true }); } catch {}

  // Make binary executable (Unix only)
  if (platform() !== 'win32') {
    const binaryPath = join(destDir, binaryName);
    if (existsSync(binaryPath)) {
      chmodSync(binaryPath, 0o755);
    }
  }

  // Verify the binary exists
  const finalBinary = join(destDir, binaryName);
  if (!existsSync(finalBinary)) {
    console.warn('  \x1b[31mpledge\x1b[0m: Binary not found after extraction');
    return tryBuildFromSource();
  }

  console.log('  \x1b[32m✓\x1b[0m Binary installed: ' + finalBinary);
  console.log('');
}

function tryBuildFromSource() {
  console.warn('  \x1b[33mpledge\x1b[0m: Attempting to build from source...');
  console.warn('  Make sure Rust is installed: https://rustup.rs');
  console.warn('');

  // Check if cargo is available
  const cargoCheck = spawnSync('cargo', ['--version'], { stdio: 'pipe' });
  if (cargoCheck.status !== 0) {
    console.warn('  \x1b[31mpledge\x1b[0m: Rust/Cargo is not installed.');
    console.warn('  Install Rust: https://rustup.rs');
    console.warn('');
    return;
  }

  // Check if Zig is available. Without this check, a missing Zig surfaces
  // as a raw `panic!("Failed to run 'zig build': ...")` deep inside
  // native-sys/build.rs mid-`cargo build` — confusing for anyone hitting
  // this source-build fallback on a platform with no prebuilt binary. See
  // PRODUCTION-READINESS-100.md goal 27.
  const zigCheck = spawnSync('zig', ['version'], { stdio: 'pipe' });
  if (zigCheck.status !== 0) {
    console.warn('  \x1b[31mpledge\x1b[0m: Zig is not installed (required to build the native library).');
    console.warn('  Install Zig: https://ziglang.org/download/');
    console.warn('  Or set ZIG_EXECUTABLE to point at an existing Zig binary.');
    console.warn('');
    return;
  }

  // Check if Cargo.toml exists in the parent directory (source checkout)
  const sourceDir = join(__dirname, '..');
  if (!existsSync(join(sourceDir, 'Cargo.toml'))) {
    console.warn('  \x1b[31mpledge\x1b[0m: Source not available in this installation.');
    console.warn('  To build from source:');
    console.warn('    git clone https://github.com/pledgeandgrow/pledgepack');
    console.warn('    cd pledgepack && cargo build --release');
    console.warn('');
    return;
  }

  const result = spawnSync('cargo', ['build', '--release'], {
    cwd: sourceDir,
    stdio: 'inherit',
  });

  if (result.status !== 0) {
    console.warn('  \x1b[31mpledge\x1b[0m: Failed to build from source.');
    console.warn('  Make sure Rust is installed: https://rustup.rs');
    console.warn('');
  }
}

downloadAndExtract().catch((err) => {
  console.warn('  \x1b[31mpledge\x1b[0m: ' + err.message);
  tryBuildFromSource();
});
