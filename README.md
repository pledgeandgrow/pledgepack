# Pledgepack

A Rust + Zig bundler and dev server. Built with Oxc for transforms, Lightning CSS for styles, Axum for the dev server, and a Zig C ABI for hot-path file I/O and SIMD scanning.

> **npm package:** `pledgepack` · **CLI command:** `pledgepack` · **Rust crates:** `pledgepack-*`
>
> **Note:** earlier versions also registered a `pledge` alias. It's been
> removed — `pledgestack` (the separate full-stack framework this bundler
> can serve as a backend for) also registers a `pledge` command, and having
> both packages claim the same global bin name meant which binary actually
> ran was non-deterministic depending on install order. Use `pledgepack`
> unambiguously from here on.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                pledgepack CLI                         │
│              (dev / build / serve / bench)            │
├─────────────────────────────────────────────────────┤
│                                                      │
│  ┌─────────────┐    ┌──────────────────────┐        │
│  │   RUST      │    │      ZIG             │        │
│  │ (Orchestrator)│   │  (Hot Paths)         │        │
│  │              │    │                       │        │
│  │ • Engine     │    │ • File I/O           │        │
│  │ • Resolver   │    │ • Module graph       │        │
│  │ • Cache      │    │ • SIMD scanning      │        │
│  │ • Dev server │    │ • Hashing            │        │
│  │ • Optimizer  │    │ • Memory-mapped I/O  │        │
│  │ • JS plugin  │    │                       │        │
│  │   host       │    │                       │        │
│  │ • Oxc transform│  │                       │        │
│  └──────┬───────┘    └──────────┬───────────┘        │
│         │     C ABI (zero-cost)  │                   │
│         └──────────┬─────────────┘                   │
│                    │                                 │
└─────────────────────────────────────────────────────┘
```

## Project Structure

```
pledgepack/
├── Cargo.toml              # Rust workspace
├── build.zig               # Zig build script
├── build.zig.zon           # Zig config
├── package.json            # npm package (bin: pledgepack only — the `pledge` alias was removed)
├── native-sys/             # Rust FFI bindings to Zig
│   ├── src/lib.rs          # C ABI bindings (Graph, read_file, find_imports)
│   └── zig/                # Zig native library
│       ├── lib.zig         # C ABI exports
│       ├── io.zig          # File I/O (mmap, thread pool)
│       ├── graph.zig       # Arena-allocated module graph
│       ├── simd.zig        # SIMD source scanning
│       └── bench.zig       # Benchmarks
├── crates/
│   ├── cli/                # CLI entry point (binary: pledge)
│   ├── core/               # Engine, config, transform pipeline, HTML, compression, analyzer
│   ├── cache/              # Function-level incremental cache (memory + disk/bincode)
│   ├── resolver/           # Module resolution (node_modules, tsconfig, exports)
│   ├── dev-server/         # Dev server + HMR + CSS HMR + error overlay + proxy
│   ├── optimizer/          # Tree shaking, code splitting, vendor/shared chunks
│   ├── js-plugin-host/     # Vite-compatible JS plugin API (QuickJS via rquickjs)
│   ├── wasm-plugin-host/   # WebAssembly plugin host (WASM module loading)
│   ├── adapter-react/      # React JSX + Fast Refresh adapter (Oxc-based)
│   ├── adapter-solid/      # Solid.js JSX adapter (Oxc-based)
│   ├── adapter-next/       # Next.js adapter (App/Pages Router, SSR, API routes)
│   ├── adapter-tanstack/   # TanStack Router adapter (file-based routing)
│   ├── adapter-pledgestack/ # PledgeStack adapter (React + Rust backend)
│   ├── task-system/        # Parallel task execution engine
│   └── task-system-macros/ # Procedural macros for task-system
└── docs/
    ├── ARCHITECTURE.md     # System architecture deep dive
    ├── CHANGELOG.md        # Development history
    ├── LIMITATIONS.md      # Known limitations
    ├── ROADMAP.md          # Roadmap
    ├── ANALYSIS.md         # Analysis docs
    ├── BENCHMARK.md        # Benchmark docs
    └── CONNECTION.md       # Connection docs
```

## Prerequisites

- [Rust](https://rustup.rs/) (stable, edition 2024)
- [Zig](https://ziglang.org/) (0.16.0+)

## Building from Source

```bash
git clone https://github.com/pledgeandgrow/pledgepack
cd pledgepack

# Build Zig native library
zig build -Doptimize=ReleaseFast

# Build Rust
cargo build --release
```

The binary is at `target/release/pledge`.

## Installation

```bash
# Global install
npm install -g pledgepack

# Or as a dev dependency
npm install --save-dev pledgepack
```

## Usage

```bash
# Development server with HMR
pledgepack dev                    # Start on port 3000
pledgepack dev --port 8080        # Custom port
pledgepack dev --open             # Auto-open browser
pledgepack dev --https            # HTTPS with self-signed certs

# Production build
pledgepack build                  # Build to .pledge/ (configurable via out_dir)
pledgepack build --watch          # Watch mode
pledgepack build --profile        # Profile build phases
pledgepack build --type-check     # TypeScript type checking
pledgepack build --check-budgets  # Bundle size budgets

# Serve production build
pledgepack serve                  # Static files on port 4000
pledgepack preview                # Alias for serve

# Project scaffolding
pledgepack create react my-app
pledgepack create vue my-app
pledgepack create svelte my-app
pledgepack create solid my-app
pledgepack create next my-app
pledgepack create tanstack my-app
pledgepack create pledgestack my-app
pledgepack create vanilla my-app
pledgepack create my-app          # Defaults to pledgestack

# Other commands
pledgepack test                   # Run tests (Vitest-compatible API)
pledgepack test --watch            # Watch mode tests
pledgepack test --ui               # Browser UI test mode
pledgepack bench                   # Benchmark build performance
pledgepack analyze                 # Bundle analyzer
pledgepack analyze --graph         # Dependency graph
pledgepack dashboard               # Build telemetry dashboard
pledgepack cache clear             # Clear disk cache
pledgepack cache stats             # Show cache statistics
pledgepack doctor                  # Diagnose build issues
pledgepack init                    # Add PledgePack to existing project
pledgepack migrate                 # Migrate from Vite/webpack/CRA
pledgepack schema                  # Generate JSON Schema for config
pledgepack generate-env-types      # Generate pledge-env.d.ts
pledgepack playground              # Interactive transform REPL
pledgepack completions --shell bash  # Shell completions
pledgepack manpages                # Man pages
```

## Configuration

### `pledge.config.ts`

```typescript
// pledge.config.ts — config file for PledgePack
// Note: defineConfig is a type-level helper for IDE autocompletion.
// In the npm package, the native binary reads this file directly.

export default {
  // App directory for file-based routing (auto-discovers pages)
  app_dir: 'app',
  // Explicit entry points (optional — use app_dir instead)
  entry: ['src/index.tsx'],
  framework: 'react',
  source_maps: true,
  env_prefix: 'PLEDGE_',
  env_dts: true,
  html_entry: 'index.html',
  compress_gzip: true,
  compress_brotli: true,
  edge_target: 'cloudflare',
  plugins: ['./plugins/my-plugin.ts'],
  image: {
    quality: 80,
    webp: true,
    avif: false,
    max_width: 1920,
    max_height: 1080,
  },
  library: {
    entry: 'src/index.ts',
    formats: ['esm', 'cjs'],
    name: 'MyLib',
    external: ['react'],
    declarations: true,
  },
  https: {
    cert: './cert.pem',
    key: './key.pem',
  },
  server_entry: 'server/index.ts',
  node_polyfills: true,
  define: {
    'process.env.NODE_ID': '"production"',
    '__VERSION__': '"1.0.0"',
  },
  watch: {
    enabled: false,
    debounce_ms: 300,
  },
  dev_server: {
    port: 3000,
    host: 'localhost',
    hmr: true,
    open: false,
    proxy: [
      { path: '/api', target: 'http://localhost:8080', rewrite: true, ws: true }
    ],
  },
  test: {
    include: ['**/*.{test,spec}.{ts,tsx,js,jsx}'],
    exclude: ['node_modules', '.pledge', 'dist'],
    environment: 'node',
    globals: false,
    setup_files: [],
    isolation: 'file',
    coverage: false,
    coverage_reporter: 'text',
    snapshot: true,
    snapshot_dir: '__snapshots__',
    update_snapshots: false,
  },
});
```

Config resolution order: `pledge.config.ts` → `pledge.config.js` → `pledge.config.mjs` → `pledge.json` → defaults.

### `.env` Files

PledgePack loads environment variables from `.env` files with the following precedence (highest first):

1. `.env.[mode].local`
2. `.env.[mode]`
3. `.env.local`
4. `.env`

Variables are injected via `import.meta.env.*`:

```typescript
const apiUrl = import.meta.env.PLEDGE_API_URL;
const isDev = import.meta.env.PLEDGE_DEV;
```

Built-in variables: `PLEDGE_DEV`, `PLEDGE_PROD`, `PLEDGE_MODE`, `MODE`, `DEV`, `PROD`, `SSR`.

## Supported Frameworks

| Framework | Status | File Types |
|-----------|--------|------------|
| **React** | Full | `.tsx`, `.jsx`, Fast Refresh, automatic JSX runtime |
| **Solid** | Full | `.tsx`, `.jsx`, automatic JSX with `solid-js` |
| **Vue** | Transform | `.vue` (SFC), scoped CSS, script setup |
| **Svelte** | Transform | `.svelte` (SFC), scoped CSS, render functions |
| **Astro** | Transform | `.astro`, frontmatter, islands-ready |
| **Next.js** | Adapter | App Router, Pages Router, API routes, SSR manifest |
| **TanStack** | Adapter | File-based routing, route tree generation |
| **PledgeStack** | Adapter (route discovery + scaffolding) | React frontend + Rust backend, `.rs`/`.psx` |
| **Vanilla TS/JS** | Full | `.ts`, `.js`, `.mjs` |

## Transform Pipeline

The transform pipeline lives in `crates/core/src/transform/` and is split into focused submodules:

- `mod.rs` — `TransformOutput` struct + `transform()` dispatcher (dispatches by `ModuleKind`)
- `js.rs` — JS/TS/JSX via Oxc, React Fast Refresh, dynamic import detection
- `css.rs` — Lightning CSS, CSS Modules, PostCSS/Tailwind, Sass/SCSS
- `assets.rs` — JSON, static assets, WASM, shaders
- `sfc.rs` — Vue, Svelte, Astro Single-File Components
- `env.rs` — Environment variables, define, import.meta.glob
- `data.rs` — MDX, GraphQL, YAML, CSV, TSV, TOML
- `utils.rs` — Source maps, Web Worker import transforms

### CSS Processing

- **Lightning CSS**: Production minification, CSS nesting transpilation, autoprefixing
- **CSS Modules**: `*.module.css` scoped class names with blake3 content hashing
- **PostCSS/Tailwind**: `@tailwind` directives, `@apply` expansion, base reset, utility classes
- **Sass/SCSS**: Compilation via `grass` crate (pure Rust)
- **Advanced CSS**: `composes` cross-file resolution, dark mode generation, custom property optimization, scoped CSS for React, nesting polyfill

### Asset Handling

- **Static assets**: `import logo from './logo.png'` → URL or base64 data URI (with `?inline`)
- **JSON**: Named exports + default export
- **WASM**: `import wasm from './module.wasm'` → `WebAssembly.instantiateStreaming`
- **Web Workers**: `new Worker(new URL('./worker.ts', import.meta.url))` patterns

### Code Splitting

- AST-based dynamic import detection via Oxc
- Relative specifiers tracked for chunk splitting
- Dynamic imports marked for separate chunk emission

## Dev Server

- On-demand transforms (each request triggers Oxc transform)
- AST-based import rewriting with string fallback
- Alias rewriting (`@/components` → `/src/components`)
- Extension fallback (`/src/utils.js` resolves to `utils.ts`)
- Import map injection for bare specifiers
- Error overlay with source context and stack traces
- CSS HMR (style tags updated without page reload)
- HTTPS support via rustls
- Dev server proxy (HTTP + WebSocket)
- Source maps in dev responses
- Auto-open browser, network URL display

## HMR

- File watcher via `notify` crate with 200ms debounce
- WebSocket endpoint at `/__pledge_hmr`
- React Fast Refresh (component state preservation)
- CSS HMR (in-place style tag updates)
- Error reporting via WebSocket
- Server-only hot reload (for SSR/API routes)

## Optimizer

- Tree shaking (reachability analysis from entry points)
- Side-effect detection (heuristic-based)
- Vendor splitting (`node_modules` → separate chunk)
- Shared splitting (modules used by 2+ entries → shared chunk)
- Scope hoisting (ESM imports preserved, modules share scope)

## Cache

- Two-tier: in-memory `DashMap` + disk `bincode` (persistent)
- Cache key: content hash + function ID + params hash (via `blake3`)
- Automatic persistence to `node_modules/.pledge-cache/`
- Cache invalidation by content hash

## Resolver

- Relative paths, bare specifiers, recursive `node_modules` lookup
- `tsconfig.json` paths support
- Package `exports` field with conditions (`import`, `require`, `browser`, `default`)
- Subpath exports, scoped packages, pattern matching
- Extension resolution: `.tsx` → `.ts` → `.jsx` → `.js` → `index.*`
- `module`, `main`, `browser` field fallbacks
- Per-(importer, specifier) DashMap caching

## JS Plugin Host

- Vite-compatible API: `resolveId`, `load`, `transform`, `transformIndexHtml`, `configureServer`, `buildStart`, `buildEnd`, `generateBundle`
- Embedded JS runtime via `rquickjs` (QuickJS bindings)
- Console support for plugin debugging
- Build integration with lifecycle hooks

## Production Output

- Writes transformed modules to `.pledge/` (default; configurable via `out_dir` in `pledge.config.ts`) preserving directory structure
- Extensions changed to `.js`
- Generates `index.html` with `<script type="module">` entry
- Content-hashed filenames for cache busting
- `manifest.json` mapping source files to output files
- Single-file bundle mode (`emit_single_file()`)
- Gzip + Brotli compression output
- Edge-ready output (Cloudflare Workers, Vercel Edge, Deno Deploy)

## Programmatic API

The native binary can be used programmatically via Rust crates:

- `pledgepack_core::BuildEngine` — Create a build engine instance
- `pledgepack_core::transform()` — Transform a single module
- `pledgepack_resolver::Resolver` — Resolve module specifiers
- `pledgepack_dev_server::serve()` — Start a dev server

> **Note:** The npm package is a binary launcher — it does not export JS functions.
> For JS-level API, use the CLI commands or write a Rust integration.

## Testing

- Vitest-compatible API: `describe`, `it`, `test`, `expect` with matchers
- Lifecycle hooks: `beforeEach`, `afterEach`, `beforeAll`, `afterAll`
- Real JS execution via `rquickjs` (QuickJS) with `console.log` and `require()` shim
- TypeScript stripping for QuickJS compatibility
- Watch mode, UI mode, snapshot testing, coverage reporting
- Mock support: `vi.fn()`, `vi.mock()`, `vi.spyOn()`, `vi.stubGlobal()`

## Other Features

- **Security**: SRI hashes, CSP generation, vulnerability scanning, license compliance
- **Node.js Polyfills**: 20 built-in modules (buffer, process, path, crypto, stream, etc.)
- **Define/Compile-time constants**: Replace identifiers with literal values at build time
- **Library mode**: ESM, CJS, UMD, IIFE output formats with external dependencies
- **Bundle analyzer**: Interactive HTML report with module sizes and chunk breakdown
- **Build profiling**: Per-phase timing (parse, transform, optimize, emit)
- **Build telemetry dashboard**: Web UI with build history and metrics
- **JSON Schema generation**: For `pledge.config.ts` IDE autocompletion
- **Service worker generation**: With caching strategies
- **i18n**: Translation catalog extraction
- **GraphQL code generation**: TypeScript types from `.graphql` schema files

## Crate Integrations

- **oxc** — JS/TS/JSX parsing, transformation, codegen, minification
- **lightningcss** — CSS minification, nesting, autoprefixing, CSS Modules
- **grass** — Pure Rust Sass/SCSS compiler
- **axum** — Dev server with WebSocket support
- **tokio** — Async runtime
- **rayon** — Parallel module transforms
- **blake3** — Content hashing for cache keys and asset hashing
- **rquickjs** — Embedded JS runtime (QuickJS bindings) for plugins and tests
- **notify** — File watcher for HMR and watch mode
- **similar** — Line-level diff for HMR
- **miette** — Graphical error diagnostics with source spans
- **schemars** — JSON Schema generation for config
- **dashmap** — Concurrent hashmap for cache and resolver
- **bincode** — Binary serialization for disk cache
- **flate2** / **brotli** — Compression output
- **reqwest** — Dev server proxy
- **rustls** / **tokio-rustls** — HTTPS dev server
- **noyalib** — YAML config parsing

## npm Scripts

```json
{
  "scripts": {
    "dev": "pledgepack dev",
    "build": "pledgepack build",
    "build:profile": "pledgepack build --profile",
    "build:watch": "pledgepack build --watch",
    "preview": "pledgepack preview",
    "serve": "pledgepack serve",
    "cache:clear": "pledgepack cache clear",
    "bench": "pledgepack bench",
    "analyze": "pledgepack analyze",
    "test": "pledgepack test",
    "test:watch": "pledgepack test --watch",
    "gen:env": "pledgepack generate-env-types"
  }
}
```

## Distribution

PledgePack is distributed via **npm only** (`npm install -g pledgepack`); the
Rust crates in this workspace are deliberately **not published to crates.io**
(`publish = false`). See [CONTRIBUTING.md](CONTRIBUTING.md#distribution--publish-policy).

## License

MIT License ([LICENSE](LICENSE)).

## Status

Current version: **1.0.0-rc.1** (release candidate). For the honest, source-verified picture of what
works and what doesn't, see
[docs/PRODUCTION-READINESS-100.md](docs/PRODUCTION-READINESS-100.md) — it is
the authoritative status document (a goal only counts as done when backed by
a CI-enforced regression test). As of 2026-09-17 the headline items are:

- `wasm-plugin-host` compiles and is fully green: the `task_transform` /
  `ast_pool` modules were wired into `pledgepack-core`, the WASI
  integration was migrated to the wasmtime-48 API, and the crate passes
  40 unit + 20 e2e tests (real WASM component, sandboxed instantiation,
  hook dispatch) — no longer excluded from CI's workspace commands.
- `cargo deny check advisories` / `cargo audit` are green (commit
  `75946ba`): wasmtime 28.0.1 → 48.0.2 cleared all 20 disclosed RUSTSEC
  advisories; rustls was bumped alongside it.
- Workspace `clippy -D warnings` is green with no crate exclusions.
- The previously `#[ignore]`d Windows heap-corruption resolver test
  (`pnpm_virtual_store_fallback_...`, goal 54) now passes — the crash was
  a symptom of the runtime-shim bug fixed in `3e5874b`.

See [docs/ROADMAP.md](docs/ROADMAP.md) for the roadmap.
See [docs/LIMITATIONS.md](docs/LIMITATIONS.md) for known limitations.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full architecture deep dive.
