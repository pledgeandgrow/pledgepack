// Deterministic synthetic project used by every tool in the benchmark.
//
// Besides raw size (N modules in a DAG), the project carries "probes" so the
// harness can judge output quality, not just speed:
//   dead-*     exports that nothing uses, reachable only through a barrel file
//   never      a file that no import reaches
//   devonly    code behind `process.env.NODE_ENV !== 'production'`
//   lazy       a dynamic import target that should land in its own chunk
//   lib-dead   unused exports of a node_modules package marked sideEffects:false
// Each probe embeds a unique marker string; the harness greps the output for it.
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

export const MARKERS = {
  deadBarrel: (i) => `__DEAD_BARREL_${i}__`,
  deadLib: (i) => `__DEAD_LIB_${i}__`,
  never: '__NEVER_IMPORTED__',
  devOnly: '__DEV_ONLY__',
  lazy: '__LAZY_CHUNK__',
};
export const BARREL_SIZE = 30;
export const LIB_SIZE = 40;

function rng(seed) {
  let s = seed >>> 0;
  return () => ((s = (Math.imul(s, 1664525) + 1013904223) >>> 0) / 2 ** 32);
}

function write(root, rel, body) {
  const p = join(root, rel);
  mkdirSync(join(p, '..'), { recursive: true });
  writeFileSync(p, body);
}

export function generateFixture(root, modules, seed = 1) {
  rmSync(root, { recursive: true, force: true });
  const rand = rng(seed);
  write(root, 'package.json', JSON.stringify({ name: 'bench-fixture', private: true, type: 'module' }, null, 2));
  write(root, 'tsconfig.json', JSON.stringify({
    compilerOptions: { target: 'ES2022', module: 'ESNext', moduleResolution: 'Bundler', allowImportingTsExtensions: true, noEmit: true, strict: true },
    include: ['src'],
  }, null, 2));

  // node_modules package, sideEffects:false, most exports unused.
  const lib = Array.from({ length: LIB_SIZE }, (_, i) =>
    `export function lib${i}(x) { return '${MARKERS.deadLib(i)}' + (x + ${i}); }`).join('\n');
  write(root, 'node_modules/fx-lib/package.json', JSON.stringify({
    name: 'fx-lib', version: '1.0.0', type: 'module', sideEffects: false, main: './index.js', module: './index.js',
    exports: { '.': { import: './index.js', default: './index.js' } },
  }, null, 2));
  write(root, 'node_modules/fx-lib/index.js',
    `${lib}\nexport function used(x) { return x * 3 + 1; }\n`);

  // Barrel with many dead exports.
  for (let i = 0; i < BARREL_SIZE; i++) {
    write(root, `src/barrel/b${i}.ts`,
      `export function b${i}(x: number): string { return '${MARKERS.deadBarrel(i)}' + (x + ${i}); }\n` +
      `export function keep${i}(x: number): number { return x * ${i + 2}; }\n`);
  }
  write(root, 'src/barrel/index.ts',
    Array.from({ length: BARREL_SIZE }, (_, i) => `export { b${i}, keep${i} } from './b${i}.ts';`).join('\n') + '\n');

  write(root, 'src/dead/never.ts', `export const never: string = '${MARKERS.never}';\n`);
  write(root, 'src/flags.ts',
    `export const isProd: boolean = process.env.NODE_ENV === 'production';\n` +
    `export function devOnly(): string {\n  if (process.env.NODE_ENV !== 'production') { return '${MARKERS.devOnly}'; }\n  return 'prod';\n}\n`);
  write(root, 'src/lazy.ts',
    `import { m0 } from './gen/m0.ts';\nexport function lazy(x: number): string { return '${MARKERS.lazy}' + m0(x); }\n`);

  // N modules in a binary tree (every module reachable, each called once) plus a
  // shared util imported by all of them (many-to-one edges, shared-chunk pressure).
  const NL = String.fromCharCode(10);
  write(root, 'src/gen/util.ts',
    'export function mix(a: number, b: number): number { return (a * 31 + b) % 1000003; }' + NL);
  for (let i = 0; i < modules; i++) {
    const ks = [2 * i + 1, 2 * i + 2].filter((j) => j < modules);
    const imports = ks.map((j) => `import { m${j} } from './m${j}.ts';`).join(NL);
    const mul = 3 + Math.floor(rand() * 90), add = Math.floor(rand() * 1000);
    const calls = ks.map((j) => `m${j}(y)`).join(' + ') || '0';
    write(root, `src/gen/m${i}.ts`, [
      `import { mix } from './util.ts';`,
      imports,
      `interface Shape${i} { readonly n: number }`,
      `export function m${i}(x: number): number {`,
      `  const s: Shape${i} = { n: x };`,
      `  const y = mix(s.n * ${mul}, ${add});`,
      `  return mix(y, ${calls});`,
      `}`, ''].join(NL));
  }

  write(root, 'src/main.ts',
    `import { m0 } from './gen/m0.ts';\nimport { keep0, keep7, keep19 } from './barrel/index.ts';\n` +
    `import { used } from 'fx-lib';\nimport { isProd, devOnly } from './flags.ts';\n\n` +
    `const total = m0(7) + keep0(3) + keep7(3) + keep19(3) + used(5);\n` +
    `const { lazy } = await import('./lazy.ts');\n` +
    `console.log(JSON.stringify({ total, isProd, dev: devOnly(), lazy: lazy(11).replace(/^[A-Z_]+/, '') }));\n`);
}

if (process.argv[1]?.endsWith('generate-fixture.mjs')) {
  const [root = 'fixtures/f300', n = '300'] = process.argv.slice(2);
  generateFixture(root, Number(n));
  console.log(`generated ${n} modules at ${root}`);
}
