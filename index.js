// pledgepack — JS entry point.
//
// The bundler itself is a native binary (see bin/pledge.js). This module only
// exists so `pledge.config.ts` files can do:
//
//   import { defineConfig } from 'pledgepack';
//   export default defineConfig({ ... });
//
// `defineConfig` is an identity function — it exists purely for typed
// autocompletion. The binary reads and validates the config file itself.

/**
 * @template {import('./index.d.ts').PledgeConfig} T
 * @param {T} config
 * @returns {T}
 */
export function defineConfig(config) {
  return config;
}

export default { defineConfig };
