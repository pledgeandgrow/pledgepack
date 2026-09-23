// One entry per bundler. Every tool is configured for the same job:
//   production mode, minified, tree-shaken, ESM output, source maps,
//   dynamic import split into its own chunk, process.env.NODE_ENV="production".
// `command()` returns the exact process to spawn (no npm/npx/cmd shims, so wall
// time is the tool itself plus its own runtime startup, and nothing else).
import { existsSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

function pkgDir(name) {
  let dir;
  try { dir = dirname(require.resolve(`${name}/package.json`)); } catch {
    // packages with an `exports` map that hides package.json: walk up from the main entry
    let p = dirname(require.resolve(name));
    while (!existsSync(join(p, 'package.json')) || JSON.parse(readFileSync(join(p, 'package.json'), 'utf8')).name !== name) p = dirname(p);
    dir = p;
  }
  return dir;
}
function pkgBin(name, bin) {
  const dir = pkgDir(name);
  const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
  const rel = typeof pkg.bin === 'string' ? pkg.bin : pkg.bin[bin ?? name];
  return { file: join(dir, rel), version: pkg.version };
}
function version(name) {
  try { return JSON.parse(readFileSync(join(pkgDir(name), 'package.json'), 'utf8')).version; } catch { return null; }
}
const node = process.execPath;

function pledgeTool(remote) {
  const suffix = remote ? '-remote' : '';
  const cfgFile = remote ? 'pledge.remote.json' : 'pledge.json';
  return {
    label: `pledgepack${suffix}`,
    out: `out-pledgepack${suffix}`,
    caches: ['node_modules/.pledge-cache', '.pledge'],
    remote,
    demo: remote,
    available: (opts) => (opts.pledgeBin && existsSync(opts.pledgeBin)) ? null : 'no pledge binary (build one or pass --pledge-bin)',
    version: (opts) => opts.pledgeVersion ?? 'unknown',
    setup(fx, opts = {}) {
      const cfg = {
        entry: ['src/main.ts'],
        outDir: `out-pledgepack${suffix}`,
        mode: 'production',
        sourceMaps: true,
        define: { 'process.env.NODE_ENV': '"production"' },
        optimize: { minify: true, treeShake: true, splitChunks: true },
      };
      if (remote) cfg.cache = { enabled: true, dir: 'node_modules/.pledge-cache', remote: { enabled: true, backend: 'http', endpoint: `http://127.0.0.1:${opts.remotePort}`, namespace: 'bench' } };
      writeFileSync(join(fx, cfgFile), JSON.stringify(cfg, null, 2));
    },
    command: (opts) => ({ cmd: opts.pledgeBin, args: ['build', '--config', cfgFile] }),
    entry: null, // resolved from manifest / heuristics in run.mjs
  };
}

/// A deliberately expensive Rollup/Vite-shaped `transform` hook. `cacheable: true`
/// opts it into pledgepack's plugin-output cache (`crates/js-plugin-host` — the
/// fingerprint covers name+version+source+cache-salt, and the result is keyed by
/// that fingerprint plus the exact source text, so it's still per-file/per-content,
/// not "run once ever"). The busy-loop stands in for real plugin work (a
/// transpiler, an image optimizer, a codegen step) expensive enough that skipping
/// it is visible against everything else the build does.
const SLOW_PLUGIN_SRC = `export default {
  name: 'slow-plugin',
  version: '1',
  cacheable: true,
  transform(code, id) {
    if (!id.endsWith('.ts') && !id.endsWith('.tsx')) return null;
    let h = 0;
    for (let i = 0; i < 80000; i++) {
      h = (h * 33 + code.charCodeAt(i % code.length)) % 1000000007;
    }
    return code + '\\n// slow-plugin:' + h;
  },
};
`;

/// pledgepack with the slow plugin above wired in. Plugin loading normally
/// requires an Ed25519 `<file>.sig.json` sidecar (`plugin_security.require_signed`,
/// default true) — disabled here (`requireSigned: false`) purely so this bench
/// fixture doesn't need a signing keypair; real projects should sign plugins or
/// explicitly opt out themselves, not inherit this from a benchmark config.
function pledgePluginTool() {
  const out = 'out-pledgepack-plugin';
  return {
    label: 'pledgepack-plugin',
    out,
    caches: ['node_modules/.pledge-cache', '.pledge'],
    demo: true,
    available: (opts) => (opts.pledgeBin && existsSync(opts.pledgeBin)) ? null : 'no pledge binary (build one or pass --pledge-bin)',
    version: (opts) => opts.pledgeVersion ?? 'unknown',
    setup(fx) {
      writeFileSync(join(fx, 'slow-plugin.js'), SLOW_PLUGIN_SRC);
      writeFileSync(join(fx, 'pledge.plugin.json'), JSON.stringify({
        entry: ['src/main.ts'],
        outDir: out,
        mode: 'production',
        sourceMaps: true,
        define: { 'process.env.NODE_ENV': '"production"' },
        optimize: { minify: true, treeShake: true, splitChunks: true },
        plugins: ['./slow-plugin.js'],
        pluginSecurity: { requireSigned: false },
      }, null, 2));
    },
    command: (opts) => ({ cmd: opts.pledgeBin, args: ['build', '--config', 'pledge.plugin.json'] }),
    entry: null,
  };
}

export const TOOLS = {
  pledgepack: pledgeTool(false),
  'pledgepack-remote': pledgeTool(true),
  'pledgepack-plugin': pledgePluginTool(),
  esbuild: {
    label: 'esbuild',
    out: 'out-esbuild',
    caches: [],
    available: () => { try { pkgDir('esbuild'); return null; } catch { return 'not installed'; } },
    version: () => version('esbuild'),
    setup() {},
    command() {
      const plat = `${process.platform}-${process.arch}`;
      let bin;
      try { const d = pkgDir(`@esbuild/${plat}`); bin = join(d, process.platform === 'win32' ? 'esbuild.exe' : 'bin/esbuild'); } catch { bin = null; }
      const common = ['src/main.ts', '--bundle', '--minify', '--splitting', '--format=esm', '--target=es2022',
        '--sourcemap', '--outdir=out-esbuild', '--entry-names=main', '--chunk-names=[name]-[hash]',
        '--define:process.env.NODE_ENV="production"', '--log-level=warning'];
      if (bin && existsSync(bin)) return { cmd: bin, args: common };
      return { cmd: node, args: [pkgBin('esbuild').file, ...common] };
    },
    entry: 'main.js',
  },
  rolldown: {
    label: 'rolldown',
    out: 'out-rolldown',
    caches: [],
    available: () => { try { pkgDir('rolldown'); return null; } catch { return 'not installed'; } },
    version: () => version('rolldown'),
    setup(fx) {
      writeFileSync(join(fx, 'rolldown.config.mjs'), `export default {
  input: 'src/main.ts',
  platform: 'browser',
  output: { dir: 'out-rolldown', format: 'esm', minify: true, sourcemap: true, entryFileNames: 'main.js', chunkFileNames: '[name]-[hash].js' },
  transform: { define: { 'process.env.NODE_ENV': '"production"' }, target: 'es2022' },
};\n`);
    },
    command: () => ({ cmd: node, args: [pkgBin('rolldown').file, '-c', 'rolldown.config.mjs'] }),
    entry: 'main.js',
  },
  rspack: {
    label: 'rspack',
    out: 'out-rspack',
    caches: ['node_modules/.cache'],
    available: () => { try { pkgDir('@rspack/core'); pkgDir('@rspack/cli'); return null; } catch { return 'not installed'; } },
    version: () => version('@rspack/core'),
    setup(fx) {
      writeFileSync(join(fx, 'rspack.config.mjs'), `import { fileURLToPath } from 'node:url';
export default {
  mode: 'production',
  target: ['web', 'es2022'],
  entry: { main: './src/main.ts' },
  devtool: 'source-map',
  experiments: { outputModule: true },
  output: { path: fileURLToPath(new URL('./out-rspack', import.meta.url)), filename: '[name].js', chunkFilename: '[name]-[contenthash:8].js', module: true, clean: true },
  resolve: { extensions: ['.ts', '.js'] },
  module: { rules: [{ test: /[.]ts$/, type: 'javascript/auto', loader: 'builtin:swc-loader', options: { jsc: { parser: { syntax: 'typescript' }, target: 'es2022' } } }] },
  optimization: { minimize: true, sideEffects: true, usedExports: true, concatenateModules: true },
  stats: 'errors-warnings',
};
`);
    },
    command: () => ({ cmd: node, args: [pkgBin('@rspack/cli', 'rspack').file, 'build', '-c', 'rspack.config.mjs'] }),
    entry: 'main.js',
  },
  vite: {
    label: 'vite',
    out: 'out-vite',
    caches: ['node_modules/.vite'],
    available: () => { try { pkgDir('vite'); return null; } catch { return 'not installed'; } },
    version: () => version('vite'),
    setup(fx) {
      writeFileSync(join(fx, 'vite.config.mjs'), `export default {
  logLevel: 'warn',
  define: { 'process.env.NODE_ENV': '"production"' },
  build: {
    outDir: 'out-vite', emptyOutDir: true, sourcemap: true, target: 'es2022', minify: true, modulePreload: false,
    rollupOptions: { input: 'src/main.ts', output: { entryFileNames: 'main.js', chunkFileNames: '[name]-[hash].js' } },
  },
};\n`);
    },
    command: () => ({ cmd: node, args: [pkgBin('vite').file, 'build'] }),
    entry: 'main.js',
  },
};

export function clean(fx, tool) {
  rmSync(join(fx, tool.out), { recursive: true, force: true });
  for (const c of tool.caches) rmSync(join(fx, c), { recursive: true, force: true });
}
