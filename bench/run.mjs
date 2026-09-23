#!/usr/bin/env node
// Comparative build benchmark. See README.md.
//   node run.mjs [--modules 300,2000] [--runs 5] [--tools pledgepack,esbuild,...]
//                [--pledge-bin path] [--timeout seconds]
import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';
import os from 'node:os';
import { BARREL_SIZE, generateFixture, LIB_SIZE, MARKERS } from './generate-fixture.mjs';
import { clean, TOOLS } from './tools.mjs';
import { startCacheServer } from './remote-server.mjs';

const here = dirname(fileURLToPath(import.meta.url));

// ---- args -------------------------------------------------------------------
const args = process.argv.slice(2);
const opt = (name, dflt) => { const i = args.indexOf(`--${name}`); return i >= 0 ? args[i + 1] : dflt; };
const sizes = opt('modules', '300,2000').split(',').map(Number);
const runs = Number(opt('runs', '5'));
// pledgepack-remote / pledgepack-plugin are opt-in demo scenarios (need a local
// cache server, or measure plugin-cache behavior with no equivalent competitor
// column) rather than part of the head-to-head comparison.
const toolNames = opt('tools', Object.keys(TOOLS).filter((t) => !TOOLS[t].demo).join(',')).split(',');
const timeoutMs = Number(opt('timeout', '300')) * 1000;

function findPledgeBin() {
  const cand = [opt('pledge-bin'), process.env.PLEDGE_BIN,
    join(here, '../target-bench/release/pledge.exe'), join(here, '../target-bench/release/pledge'),
    join(here, '../target/release/pledge.exe'), join(here, '../target/release/pledge')].filter(Boolean);
  return cand.map((p) => resolve(p)).find((p) => existsSync(p));
}
const pledgeBin = findPledgeBin();
let pledgeVersion;
let pledgeProfile = 'release';
if (pledgeBin) {
  const v = spawnSync(pledgeBin, ['--version'], { encoding: 'utf8' });
  pledgeVersion = (v.stdout || '').trim().replace(/^pledgepack\s*/, '') || 'unknown';
  if (/[\\/]debug[\\/]/.test(pledgeBin)) pledgeProfile = 'DEBUG';
}
const toolOpts = { pledgeBin, pledgeVersion: pledgeVersion && `${pledgeVersion} (${pledgeProfile})` };

// ---- helpers ----------------------------------------------------------------
const median = (a) => { const s = [...a].sort((x, y) => x - y); const m = s.length >> 1; return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2; };
const fmtMs = (ms) => (ms == null ? 'n/a' : ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${ms.toFixed(0)} ms`);
const fmtKB = (b) => (b == null ? 'n/a' : `${(b / 1024).toFixed(1)} KB`);

function walk(dir) {
  const out = [];
  if (!existsSync(dir)) return out;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) out.push(...walk(p));
    else out.push(p);
  }
  return out;
}

// Async (spawn), not spawnSync: the pledgepack-remote scenario runs an HTTP
// server in this same process (remote-server.mjs), and spawnSync blocks the
// whole Node.js event loop until the child exits — starving that in-process
// server of the event-loop time it needs to ever respond, so every request
// pledge.exe makes to it stalls until pledgepack's own client-side timeout
// (or run.mjs's own --timeout kills the child first). Not a pledgepack bug:
// a `pledge.exe build` run directly against the same server, outside this
// harness, completes normally. spawn() keeps the event loop free.
function timedRun(tool, fx) {
  const { cmd, args: a } = tool.command(toolOpts);
  const t0 = performance.now();
  return new Promise((resolvePromise) => {
    let child;
    try {
      child = spawn(cmd, a, {
        cwd: fx,
        env: { ...process.env, NODE_ENV: 'production', CI: '1', FORCE_COLOR: '0', NO_COLOR: '1' },
      });
    } catch (err) {
      resolvePromise({ ms: performance.now() - t0, ok: false, status: null, signal: null, err: err.message, stderr: '' });
      return;
    }
    let out = '';
    let timedOut = false;
    const chunks = [];
    let bytes = 0;
    const onData = (buf) => {
      bytes += buf.length;
      if (bytes <= 64 << 20) chunks.push(buf);
    };
    child.stdout?.on('data', onData);
    child.stderr?.on('data', onData);
    const timer = setTimeout(() => { timedOut = true; child.kill('SIGKILL'); }, timeoutMs);
    child.on('error', (err) => {
      clearTimeout(timer);
      resolvePromise({ ms: performance.now() - t0, ok: false, status: null, signal: null, err: err.message, stderr: Buffer.concat(chunks).toString('utf8') });
    });
    child.on('close', (status, signal) => {
      clearTimeout(timer);
      out = Buffer.concat(chunks).toString('utf8');
      resolvePromise({
        ms: performance.now() - t0, ok: status === 0 && !timedOut, status, signal,
        err: timedOut ? `timed out after ${timeoutMs}ms` : undefined, stderr: out,
      });
    });
  });
}

function findEntry(tool, outDir) {
  if (tool.entry && existsSync(join(outDir, tool.entry))) return join(outDir, tool.entry);
  const mf = join(outDir, 'manifest.json');
  if (existsSync(mf)) {
    try {
      const m = JSON.parse(readFileSync(mf, 'utf8'));
      const flat = m.files ?? m.modules ?? m;
      for (const v of Object.values(flat)) {
        if (v && typeof v === 'object' && v.is_entry && v.file && existsSync(join(outDir, v.file))) return join(outDir, v.file);
      }
      for (const [k, v] of Object.entries(flat)) {
        if (/src[\\/]main\.(ts|js)$/.test(k)) {
          const f = typeof v === 'string' ? v : v.file ?? v.output ?? v.path;
          if (f && existsSync(join(outDir, f))) return join(outDir, f);
        }
      }
    } catch { /* fall through */ }
  }
  const js = walk(outDir).filter((f) => /\.m?js$/.test(f));
  return js.find((f) => /(^|[\\/])main([.-][^\\/]*)?\.m?js$/.test(f)) ?? js.find((f) => /index/.test(basename(f))) ?? null;
}

function analyze(tool, fx, expected) {
  const outDir = join(fx, tool.out);
  const files = walk(outDir);
  const js = files.filter((f) => /\.(m|c)?js$/.test(f));
  const maps = files.filter((f) => f.endsWith('.map'));
  const bufs = js.map((f) => readFileSync(f));
  const text = bufs.map((b) => b.toString('utf8')).join('\n');
  const gz = bufs.reduce((n, b) => n + gzipSync(b, { level: 9 }).length, 0);
  const br = bufs.reduce((n, b) => n + brotliCompressSync(b, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }).length, 0);
  const entryFile = findEntry(tool, outDir);

  let deadBarrel = 0;
  for (let i = 0; i < BARREL_SIZE; i++) if (text.includes(MARKERS.deadBarrel(i))) deadBarrel++;
  let deadLib = 0;
  for (let i = 0; i < LIB_SIZE; i++) if (text.includes(MARKERS.deadLib(i))) deadLib++;
  const lazyFile = js.find((f) => readFileSync(f, 'utf8').includes(MARKERS.lazy));

  // Does the output run, and does it compute the same answer as the source?
  //
  // When the tool emitted an index.html, its <script> tags are the real,
  // intended load order/mechanism — including any that are root-absolute
  // (`/chunk-abc.js`), meant for HTTP serving and resolved by a browser
  // against the page's origin. Node's own resolver treats a leading `/` as
  // a filesystem path, so a plain `node entryFile` can't follow those; a
  // driver script that imports each <script> in order, through
  // root-url-loader.mjs (which redirects `/x` to `<outDir>/x`, matching
  // what a browser would do), reproduces that load order faithfully without
  // needing an actual server or browser. Falls back to a one-line driver
  // that just imports entryFile when there's no index.html to read a load
  // order from.
  //
  // The `--import <hook>` argument must be a path *relative to cwd*, not
  // absolute: an absolute Windows path there (e.g. `C:\...\register-root-
  // url-loader.mjs`) breaks Node's *separate* main-script resolution with
  // ERR_UNSUPPORTED_ESM_URL_SCHEME ("Received protocol 'c:'"), reproduced in
  // isolation and unrelated to root-url-loader.mjs's own logic (nothing in
  // it runs before the crash). A relative `--import` path doesn't trigger
  // it, regardless of whether the main script argument itself is relative
  // or absolute — this looks like a genuine Node-on-Windows quirk, not
  // something to fix here, just route around.
  let run = { ok: false, note: 'no entry file found' };
  if (entryFile) {
    writeFileSync(join(outDir, 'package.json'), '{"type":"module"}\n');
    const htmlPath = join(outDir, 'index.html');
    let srcs = [pathToFileURL(entryFile).href];
    if (existsSync(htmlPath)) {
      const html = readFileSync(htmlPath, 'utf8');
      const htmlSrcs = [...html.matchAll(/<script\b[^>]*\bsrc=["']([^"']+)["']/gi)].map((m) => m[1]);
      if (htmlSrcs.length) srcs = htmlSrcs;
    }
    const driver = join(outDir, '__bench_driver.mjs');
    writeFileSync(driver, srcs.map((s) => `await import(${JSON.stringify(s)});`).join('\n') + '\n');
    let loaderRel = relative(outDir, join(here, 'register-root-url-loader.mjs')).split('\\').join('/');
    if (!loaderRel.startsWith('.')) loaderRel = `./${loaderRel}`; // avoid it reading as a bare specifier
    const r = spawnSync(process.execPath, ['--import', loaderRel, driver], {
      cwd: outDir, encoding: 'utf8', timeout: 60000,
      env: { ...process.env, NODE_ENV: 'production', BENCH_OUT_DIR: outDir },
    });
    const got = (r.stdout || '').trim();
    if (r.status === 0 && got === expected) run = { ok: true };
    else if (r.status !== 0) run = { ok: false, note: ((r.stderr || '').split('\n').find((l) => /Error/.test(l)) ?? `exit ${r.status}`).trim().slice(0, 160) };
    else run = { ok: false, note: `wrong output: ${got.slice(0, 80)}` };
  }
  return {
    files: files.filter((f) => !f.endsWith('.map') && !/package\.json$/.test(f)).length,
    jsFiles: js.length,
    jsBytes: bufs.reduce((n, b) => n + b.length, 0), gzip: gz, brotli: br,
    sourcemaps: maps.length > 0 || /sourceMappingURL=/.test(text),
    deadBarrelSurvivors: deadBarrel, deadLibSurvivors: deadLib,
    neverImportedPresent: text.includes(MARKERS.never),
    devOnlyPresent: text.includes(MARKERS.devOnly),
    lazySplit: !!lazyFile && !!entryFile && lazyFile !== entryFile,
    run,
  };
}

// ---- main -------------------------------------------------------------------
const results = { meta: { date: new Date().toISOString(), node: process.version, os: `${os.type()} ${os.release()} ${os.arch()}`, cpu: os.cpus()[0]?.model, cores: os.cpus().length, runs, sizes }, sizes: {} };
console.log(`Node ${process.version} · ${results.meta.cpu} (${results.meta.cores} cores) · ${runs} runs/config`);
if (pledgeProfile === 'DEBUG') console.warn('WARNING: pledgepack binary is a DEBUG build; timings are not comparable. Build with `cargo build --release`.');

for (const n of sizes) {
  const fx = resolve(here, 'fixtures', `f${n}`);
  generateFixture(fx, n);
  const oracle = spawnSync(process.execPath, ['--experimental-strip-types', '--no-warnings', 'src/main.ts'], { cwd: fx, encoding: 'utf8', env: { ...process.env, NODE_ENV: 'production' } });
  if (oracle.status !== 0) { console.error('oracle failed:', oracle.stderr); process.exit(1); }
  const expected = oracle.stdout.trim();
  console.log(`\n== ${n} modules · expected: ${expected}`);
  results.sizes[n] = {};

  for (const name of toolNames) {
    const tool = TOOLS[name];
    if (!tool) { console.error(`unknown tool ${name}`); continue; }
    const why = tool.available(toolOpts);
    if (why) { console.log(`  ${name}: skipped (${why})`); results.sizes[n][name] = { skipped: why }; continue; }
    const server = tool.remote ? await startCacheServer() : null;
    tool.setup(fx, { remotePort: server?.port });
    const rec = { version: tool.version(toolOpts), cold: [], warm: [] };
    let failure = null;

    // cold: caches and output wiped before every run; one discarded run first for OS file-cache parity
    clean(fx, tool);
    const first = await timedRun(tool, fx);
    if (!first.ok) failure = first;
    for (let i = 0; i < runs && !failure; i++) {
      clean(fx, tool);
      const r = await timedRun(tool, fx);
      if (!r.ok) { failure = r; break; }
      rec.cold.push(r.ms);
    }
    // warm: persistent caches kept, only the output dir removed (not meaningful for the remote variant)
    if (!failure && !tool.remote) {
      clean(fx, tool);
      await timedRun(tool, fx);
      for (let i = 0; i < runs; i++) {
        rmSync(join(fx, tool.out), { recursive: true, force: true });
        const r = await timedRun(tool, fx);
        if (!r.ok) { failure = r; break; }
        rec.warm.push(r.ms);
      }
    }
    if (failure) {
      rec.error = `${failure.err ?? `exit ${failure.status}${failure.signal ? ` (${failure.signal})` : ''}`}: ${failure.stderr.trim().split('\n').slice(-4).join(' | ').slice(0, 300)}`;
      console.log(`  ${name}: FAILED — ${rec.error}`);
      results.sizes[n][name] = rec;
      await server?.close();
      continue;
    }
    clean(fx, tool);
    await timedRun(tool, fx);
    rec.output = analyze(tool, fx, expected);
    rec.coldMedian = median(rec.cold); rec.coldMin = Math.min(...rec.cold);
    rec.warmMedian = rec.warm.length ? median(rec.warm) : null; rec.warmMin = rec.warm.length ? Math.min(...rec.warm) : null;
    if (server) rec.remote = { ...server.stats };
    results.sizes[n][name] = rec;
    await server?.close();
    console.log(`  ${name.padEnd(11)} cold ${fmtMs(rec.coldMedian).padStart(9)}  warm ${fmtMs(rec.warmMedian).padStart(9)}  js ${fmtKB(rec.output.jsBytes).padStart(10)}  gz ${fmtKB(rec.output.gzip).padStart(9)}  runs:${rec.output.run.ok ? 'ok' : 'FAIL'}`);
  }
}

// ---- report -----------------------------------------------------------------
let md = `# Build benchmark results\n\n${results.meta.date} · Node ${process.version} · ${results.meta.os} · ${results.meta.cpu} (${results.meta.cores} cores) · median of ${runs} runs\n\n`;
for (const n of sizes) {
  const rows = Object.entries(results.sizes[n]);
  md += `## ${n} modules\n\n| tool | version | cold build | warm build | JS output | gzip | brotli | JS files | runs in Node and correct |\n|---|---|---|---|---|---|---|---|---|\n`;
  for (const [name, r] of rows) {
    if (r.skipped) { md += `| ${name} | - | skipped: ${r.skipped} | | | | | | |\n`; continue; }
    if (r.error) { md += `| ${name} | ${r.version} | FAILED | | | | | | ${r.error.slice(0, 80).replace(/\|/g, '/')} |\n`; continue; }
    const o = r.output;
    md += `| ${name} | ${r.version} | ${fmtMs(r.coldMedian)} | ${fmtMs(r.warmMedian)} | ${fmtKB(o.jsBytes)} | ${fmtKB(o.gzip)} | ${fmtKB(o.brotli)} | ${o.jsFiles} | ${o.run.ok ? 'yes' : `**no**: ${String(o.run.note).replace(/\|/g, '/')}`} |\n`;
  }
  md += `\n**Output quality probes (${n} modules)**\n\n| tool | dead barrel exports left (of ${BARREL_SIZE}) | dead lib exports left (of ${LIB_SIZE}) | unreachable file emitted | dev-only branch kept | dynamic import split | source maps |\n|---|---|---|---|---|---|---|\n`;
  for (const [name, r] of rows) {
    if (!r.output) continue;
    const o = r.output;
    md += `| ${name} | ${o.deadBarrelSurvivors} | ${o.deadLibSurvivors} | ${o.neverImportedPresent ? 'yes' : 'no'} | ${o.devOnlyPresent ? 'yes' : 'no'} | ${o.lazySplit ? 'yes' : 'no'} | ${o.sourcemaps ? 'yes' : 'no'} |\n`;
  }
  md += '\n';
}
md += 'Lower is better except "runs in Node and correct", "dynamic import split" and "source maps". Warm = persistent caches kept; tools without a persistent build cache show warm ~ cold.\n';
mkdirSync(join(here, 'results'), { recursive: true });
const stamp = results.meta.date.replace(/[:.]/g, '-');
writeFileSync(join(here, 'results', `results-${stamp}.json`), JSON.stringify(results, null, 2));
writeFileSync(join(here, 'results', `results-${stamp}.md`), md);
writeFileSync(join(here, 'results', 'latest.md'), md);
console.log(`\n${md}\nSaved to bench/results/results-${stamp}.{json,md}`);

// ---- gate -------------------------------------------------------------------
// `--gate` exits non-zero if pledgepack regresses on correctness/quality.
// Timing is only gated when you pass `--max-cold-ratio <n>` (pledgepack cold
// median / fastest other tool's cold median), because wall time is noisy.
if (args.includes('--gate')) {
  const maxRatio = opt('max-cold-ratio') ? Number(opt('max-cold-ratio')) : null;
  const failures = [];
  for (const n of sizes) {
    const r = results.sizes[n].pledgepack;
    if (!r || r.skipped) { failures.push(`${n}: pledgepack did not run`); continue; }
    if (r.error) { failures.push(`${n}: build failed: ${r.error}`); continue; }
    const o = r.output;
    if (!o.run.ok) failures.push(`${n}: output does not run correctly (${o.run.note})`);
    if (o.deadBarrelSurvivors) failures.push(`${n}: ${o.deadBarrelSurvivors} dead barrel exports survive`);
    if (o.deadLibSurvivors) failures.push(`${n}: ${o.deadLibSurvivors} dead package exports survive`);
    if (o.devOnlyPresent) failures.push(`${n}: NODE_ENV dev-only branch kept`);
    if (o.neverImportedPresent) failures.push(`${n}: unreachable file emitted`);
    if (!o.lazySplit) failures.push(`${n}: dynamic import not split into its own chunk`);
    if (!o.sourcemaps) failures.push(`${n}: no source maps`);
    if (maxRatio != null) {
      const others = Object.entries(results.sizes[n]).filter(([k, v]) => k !== 'pledgepack' && !k.startsWith('pledgepack-') && v.coldMedian);
      if (others.length) {
        const best = Math.min(...others.map(([, v]) => v.coldMedian));
        const ratio = r.coldMedian / best;
        if (ratio > maxRatio) failures.push(`${n}: cold build ${ratio.toFixed(2)}x the fastest competitor (limit ${maxRatio}x)`);
      }
    }
  }
  if (failures.length) { console.error(`\nGATE FAILED:\n - ${failures.join('\n - ')}`); process.exit(1); }
  console.log('\nGATE PASSED');
}
