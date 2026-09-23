// A Node module resolve hook that treats OUT_DIR (env var) as the site root
// for any specifier starting with `/` — matches how a browser resolves a
// root-absolute asset URL (`/chunk-abc.js`) against the page's origin.
// Bundlers commonly emit these for HTML <script src> tags and for dynamic
// `import()` targets meant to be deployed under a known base path; Node's
// own resolver treats a leading `/` as a filesystem-root path instead, so
// without this hook such a specifier resolves to (and fails to find) e.g.
// `C:\chunk-abc.js` rather than `<outDir>/chunk-abc.js`.
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';

export async function resolve(specifier, context, nextResolve) {
  if (specifier.startsWith('/') && process.env.BENCH_OUT_DIR) {
    return { url: pathToFileURL(join(process.env.BENCH_OUT_DIR, specifier.slice(1))).href, shortCircuit: true };
  }
  return nextResolve(specifier, context);
}
