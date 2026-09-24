# Pledge Architecture

> **2026-09-15 correction:** a source-level audit found two factual errors
> below, now fixed inline: this document said "Wasmtime v47" in two places —
> `Cargo.lock` actually pins `wasmtime`/`wasmtime-wasi` at **28.0.1**, which
> carries multiple disclosed RUSTSEC advisories (see
> [`PRODUCTION-READINESS-100.md`](PRODUCTION-READINESS-100.md) Phase 1 and
> [`LIMITATIONS.md`](LIMITATIONS.md)); and the WIT contract version
> references here (v0.1.0/v0.1.1) are stale — it's at **v0.1.3** as of this
> session (a `handle-hot-update` hook was added, see
> `PRODUCTION-READINESS-100.md` Phase 3 goal 43). Treat other specific
> version numbers and "Done"/"Implemented" claims elsewhere in this document
> as unverified in this pass — `PRODUCTION-READINESS-100.md` is the
> currently test-verified source of truth for what's actually working.
>
> **2026-09-17 update:** the wasmtime 28 → 48 bump has landed (commit
> `75946ba` — `Cargo.lock` now pins 48.0.2, clearing all 20 RUSTSEC
> advisories on 28.0.1; `cargo audit`/`cargo deny` pass), and
> `wasm-plugin-host`'s compile break is fixed: `task_transform`/`ast_pool`
> were wired into `pledgepack-core` and the WASI code migrated to the 48
> API — the crate now compiles and passes all tests (40 unit + 20 e2e) on
> wasmtime 48.0.2, and is back in CI's workspace commands. Stray "wasmtime
> 28.0.1" references below describe the pre-bump state.
>
> **0.4.0 update:** production emit was rewritten — modules are lowered
> into `__pp.def`/`__pp.req` records wrapped in the `__pp` runtime
> (`crates/core/src/bundle.rs`), chunks are content-hashed files keyed in
> `manifest.json`, and emitted output is scanned for secrets. The
> duplicated engine resolver was unified onto `pledgepack-resolver`, and
> `dep_bundler`/`watcher`/`lazy_pipeline`/`middleware`/`hmr_diff` now live
> in `dev-server`, `remote`/`git_cache` in `cache` (paths below updated).

## Overview

Pledge is a hybrid Rust + Zig bundler that uses:
- **Zig** for hot paths (file I/O, module graph, SIMD scanning, hashing) via C ABI
- **Rust** for orchestration (build engine, resolver, cache, dev server, optimizer, plugin host)
- **Oxc** for JavaScript/TypeScript/JSX transformation (replacing SWC/esbuild)
- **QuickJS** (rquickjs 0.12.2) for JS plugin execution (Vite-compatible plugin API, ES2020 compliant)

## Data Flow

```
User source files (src/*.tsx, *.ts)
        │
        ▼
   BuildEngine
   ├── Resolver ──► resolves import specifiers to file paths
   ├── Zig I/O ────► read_file() via C ABI (mmap, thread pool)
   ├── Transform ──► Oxc: parse → semantic → transform → codegen
   ├── Cache ──────► memory (DashMap) → disk (bincode) → transform
   └── Graph ──────► Zig arena-allocated module graph (0B/node)
        │
        ▼
   Optimizer
   ├── Tree shaking (reachability + export-demand shake → stub registry)
   ├── Chunk splitting (entry / vendor / async / shared)
   └── Manual chunks + route splitting
        │
        ▼
   Emitter ──► out_dir (`__pp` runtime chunks, manifest.json, index.html)
```

## Crate Dependency Graph

```
pledgepack-cli
├── pledgepack-core (engine, bundle, config, config_validate, transform/*, pipeline, env, html, compression, analyzer, edge, dep_bundler, polyfills, transform_optimizations, css_features, css_advanced, css_frameworks, css_in_js, tailwind_v4, postcss, asset_pipeline, image_pipeline, fonts, svg, favicons, plugin_system, plugin_hooks, plugin_registry, plugin_docs, plugin_schema, plugin_template, plugin_types, output_distribution, service_worker, lsp_server, migrate, module_graph, doctor, diagnostics, export_check, js_config, telemetry, budgets, bench, webhooks, i18n, rtl, a11y, encrypt, advanced, ecosystem, security, determinism, api, detect, drizzle, estree, examples_gallery, playground, prisma, router, sourcemap_compose, task_transform, type_check, visual_regression, module, paths, ast_pool, presets)
│   ├── pledgepack-cache (function-level cache, memory + disk)
│   ├── pledgepack-native-sys (FFI to Zig)
│   ├── pledgepack-resolver (single resolution implementation — engine delegates to it)
│   ├── oxc (parser, semantic, transformer, codegen)
│   ├── lightningcss (CSS minification, CSS Modules)
│   ├── blake3 (content hashing for CSS Modules + cache keys)
│   ├── flate2 (gzip compression)
│   ├── brotli (Brotli compression)
│   ├── rayon (parallel transforms, parallel plugin execution)
│   └── dashmap (concurrent cache, concurrent plugin registry)
├── pledgepack-dev-server (axum, notify, tokio-tungstenite, reqwest, rustls; fs_guard, origin_guard, watcher, hmr_diff, lazy_pipeline, middleware, plugin_hooks, shell_generator)
│   ├── pledgepack-core
│   ├── pledgepack-native-sys
│   └── oxc
├── pledgepack-cache (function-level cache, memory + disk; remote.rs S3/GCS/HTTP backends, git_cache.rs tree-hash invalidation)
├── pledgepack-optimizer (tree shaking, chunk splitting)
│   └── pledgepack-core
├── pledgepack-resolver (tsconfig, exports, node_modules)
├── pledgepack-js-plugin-host (QuickJS/rquickjs JS runtime, Vite-compatible plugin API)
├── pledgepack-adapter-react (Oxc JSX, Fast Refresh)
├── pledgepack-adapter-solid (Oxc JSX, solid-js automatic runtime)
├── pledgepack-adapter-next (App/Pages Router, SSR, API routes)
├── pledgepack-adapter-tanstack (file-based routing, route tree)
├── pledgepack-adapter-pledgestack (React frontend + Rust backend, .rs/.psx support)
├── axum + tower-http (serve/analyze commands)
└── tokio (async runtime)
```

## External Dependencies

### Workspace Dependencies (shared across crates)

| # | Crate | Version | Category | Used By |
|---|-------|---------|----------|---------|
| 1 | `serde` | 1 (derive) | Serialization | core, cli, dev-server, cache, resolver, js-plugin-host, optimizer, adapter-react |
| 2 | `serde_json` | 1 | Serialization | core, cli, dev-server, cache, resolver, js-plugin-host, optimizer, adapter-react |
| 3 | `bincode` | 2 (serde) | Binary serialization | core, cache |
| 4 | `tokio` | 1 (full) | Async runtime | core, cli, dev-server, cache |
| 5 | `axum` | 0.8 | HTTP server | cli, dev-server |
| 6 | `tower-http` | 0.6 (fs, cors) | HTTP middleware | cli, dev-server |
| 7 | `tokio-tungstenite` | 0.26 | WebSocket (HMR) | dev-server |
| 8 | `oxc` | 0.141 (full) | JS/TS/JSX compiler | core, dev-server, adapter-react |
| 9 | `lightningcss` | 1.0.0-alpha.72 | CSS engine | core |
| 10 | `blake3` | 1 | Hashing (cache keys) | core, cache |
| 11 | `base64` | 0.22 | Base64 encoding | core |
| 12 | `image` | 0.25 (jpeg, png, webp, gif) | Image processing | core |
| 13 | `tracing` | 0.1 | Logging | all crates |
| 14 | `tracing-subscriber` | 0.3 (env-filter) | Logging setup | cli |
| 15 | `anyhow` | 1 | Error handling | core, cli, dev-server, cache, resolver, js-plugin-host, optimizer, adapter-react |
| 16 | `thiserror` | 2 | Typed errors | core |
| 17 | `clap` | 4 (derive) | CLI parsing | cli |
| 18 | `clap_complete` | 4 | Shell completions | cli |
| 19 | `indicatif` | 0.18 | Progress bars | cli |
| 20 | `inquire` | 0.7 | Interactive prompts | cli |
| 21 | `notify` | 8 | File watching | core, cli, dev-server |
| 22 | `notify-debouncer-full` | 0.7 | Debounced file watching | core, dev-server |
| 23 | `libc` | 0.2 | C library bindings (Linux) | dev-server |
| 24 | `rayon` | 1 | Parallelism | core, cli, optimizer |
| 25 | `dashmap` | 6 | Concurrent HashMap | core, cache, resolver |
| 26 | `mimalloc` | 0.1 | Global allocator | cli |
| 27 | `tikv-jemallocator` | 0.6 (profiling) | Alt allocator (jemalloc) | cli (optional) |
| 28 | `camino` | 1 | Typed UTF-8 paths | cli |
| 29 | `globset` | 0.4 | Glob pattern matching | core, cli, optimizer |
| 30 | `regex` | 1 | Regex engine | core |
| 31 | `memmap2` | 0.9 | Memory-mapped I/O | core, cache |
| 32 | `comfy-table` | 7 | CLI tables | core, cli |
| 33 | `serde_yml` | 0.0.12 | YAML parsing | core |
| 34 | `miette` | 7 (fancy) | Error diagnostics | core, cli |
| 35 | `clap_mangen` | 0.2 | Man page generation | cli |
| 36 | `humansize` | 2 | File size formatting | core |
| 37 | `similar` | 2 (text) | Diff algorithm (HMR patches) | dev-server |
| 38 | `opener` | 0.7 | Cross-platform browser opening | dev-server |
| 39 | `local-ip-address` | 0.6 | Network IP detection | dev-server |
| 40 | `schemars` | 1 | JSON Schema generation (config) | core, cli |
| 41 | `grass` | 0.13 | Pure Rust Sass/SCSS compiler | core |
| 42 | `toml` | 0.8 | TOML parsing | core |
| 43 | `ureq` | 2 (json) | HTTP client (plugin registry) | core |

### Sub-crate Local Dependencies (not in workspace)

| # | Crate | Version | Used By | Purpose |
|---|-------|---------|---------|---------|
| 44 | `reqwest` | 0.12 (rustls-tls) | dev-server | HTTP client (proxy). Uses rustls-tls, no openssl dependency |
| 45 | `rustls` | 0.23 | dev-server | TLS |
| 46 | `rustls-pemfile` | 2 | dev-server | TLS cert parsing |
| 47 | `tokio-rustls` | 0.26 | dev-server | Async TLS |
| 48 | `futures-util` | 0.3 | dev-server | Async utilities |
| 49 | `flate2` | 1 | core | Gzip compression |
| 50 | `brotli` | 7 | core | Brotli compression |
| 51 | `chrono` | 0.4 | core | Date/time formatting |
| 52 | `dialoguer` | 0.11 | cli | Interactive dialogs |
| 53 | `console` | 0.15 | cli | Terminal styling |
| 54 | `atty` | 0.2 | cli | TTY detection |
| 55 | `rquickjs` | 0.12.2 | js-plugin-host | QuickJS JS engine for plugins & tests |
| 56 | `windows-sys` | 0.61 | dev-server (Windows only) | Win32 API |
| 57 | `bytemuck` | 1.21 | dev-server (Windows only) | Byte casting |

### Build Profile

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "unwind"
strip = true

[profile.dev]
opt-level = 0
incremental = true
```

**Summary:** 57 external crates + 16 internal crates = 73 total packages. All additions are pure replacements of manual code or new capabilities. No dependency conflicts or version mismatches. Workspace uses resolver v2 for feature unification.

## Zig Native Layer (`native-sys/` + `native-sys/zig/*.zig`)

### C ABI Exports
- `graph_create()`, `graph_add_module()`, `graph_add_dependency()` — Arena-allocated module graph
- `read_file(path) → bytes` — Memory-mapped file I/O with thread pool fallback
- `find_imports(source) → Vec<String>` — SIMD-accelerated import scanning
- `hash_content(source) → u64` — Content hashing for cache keys
- `___chkstk_ms` — Windows x86_64 stack probing, MSVC builds only (MinGW's runtime provides its own; `#[cfg(target_env = "msvc")]`-gated since 2026-09-15 after the GNU-toolchain segfault fix)

### Key Design Decisions
- **Arena allocation**: Module graph nodes have zero per-node allocation overhead
- **SIMD scanning**: Import specifiers found via 32-byte SIMD pattern matching
- **io_uring / IOCP**: Async file I/O on Linux (io_uring) and Windows (IOCP via thread pool)
- **Stack probing**: `___chkstk_ms` shim for MSVC builds only; MinGW toolchains use the runtime-provided symbol

## Rust Orchestration Layer

### BuildEngine (`crates/core/src/engine.rs`)
- BFS module graph traversal from entry point
- Per-module: resolve → read → transform → cache → enqueue dependencies
- Two-tier cache: memory (`HashMap`) → disk (`FunctionCache` with bincode)
- Emits content-hashed `__pp` runtime chunks + `manifest.json` + `index.html` to `config.out_dir` (dev mode serves transformed modules on demand instead)

### Transform Pipeline (`crates/core/src/transform/`)

The transform module is split into focused submodules:
- `mod.rs` — `TransformOutput` struct + `transform()` dispatcher (dispatches by `ModuleKind`)
- `js.rs` — JS/TS/JSX via Oxc, React Fast Refresh, dynamic import detection
- `css.rs` — Lightning CSS, CSS Modules, PostCSS/Tailwind, Sass
- `assets.rs` — JSON, static assets, WASM, shaders
- `sfc.rs` — Vue, Svelte, Astro Single-File Components
- `env.rs` — Environment variables, define, import.meta.glob
- `data.rs` — MDX, GraphQL, YAML, CSV, TSV, TOML
- `utils.rs` — Source maps, Web Worker import transforms
```
Source string
    │
    ├── Oxc Parser (SourceType from file extension)
    │       → AST (Program)
    │
    ├── Oxc SemanticBuilder
    │       → SymbolTable + ScopeTree
    │
    ├── Oxc Transformer (framework-aware JSX, TS type stripping)
    │       → React: JsxRuntime::Automatic (react/jsx-runtime)
    │       → Solid: JsxRuntime::Automatic, import_source=solid-js
    │       → Vue: JsxRuntime::Automatic, import_source=vue
    │       → Transformed AST
    │
    ├── Oxc Minifier (production only)
    │       → Dead code elimination, variable mangling
    │
    ├── Oxc Codegen (optional minify)
    │       → JavaScript string
    │
    └── Post-processing
            ├── Environment variable replacement (import.meta.env.PLEDGE_*)
            ├── Define replacement (compile-time constants from config.define)
            ├── import.meta.glob expansion (glob-based file imports for dynamic route/component discovery)
            ├── Dynamic import detection (Oxc AST ImportExpression visitor)
            ├── Web Worker transform (Worker + SharedWorker patterns, ?worker/?sharedworker suffixes)
            ├── Web Component compilation (.wc.tsx → Custom Elements with Shadow DOM)
            └── React Fast Refresh injection (dev mode, React only)
```

### Framework Adapters

#### Vue SFC (`transform/sfc.rs:transform_vue`)
- Extracts `<template>`, `<script setup>`, `<style scoped>` blocks
- TypeScript transform: `<script lang="ts">` blocks transformed with Oxc (type stripping)
- Compiles template to render function
- Extracts scoped CSS with `[data-v-pledge]` attribute selectors
- HMR boundary: `import.meta.hot.accept()` injected in dev mode

#### Svelte (`transform/sfc.rs:transform_svelte`)
- Extracts `<script>`, `<style>`, and markup from `.svelte` files
- TypeScript transform: `<script lang="ts">` blocks transformed with Oxc (type stripping)
- Generates DOM render function with mount/unmount lifecycle
- Scoped CSS with `[svelte-pledge]` attribute selectors
- HMR boundary: `import.meta.hot.accept()` injected in dev mode

#### Astro (`transform/sfc.rs:transform_astro`)
- Parses `---` frontmatter delimiters
- TypeScript transform: Frontmatter TS transformed with Oxc (type stripping)
- Compiles template to async render function
- Extracts `<style>` blocks as CSS
- HMR boundary: `import.meta.hot.accept()` injected in dev mode

#### Next.js (`crates/adapter-next/`)
- App Router: scans `app/` for page.tsx, layout.tsx, loading.tsx, error.tsx
- Pages Router: scans `pages/` for index.tsx, [param].tsx
- Generates client-side router with lazy imports
- Generates SSR manifest (JSON)

#### TanStack (`crates/adapter-tanstack/`)
- Scans `src/routes/` for file-based routes
- `$param` files → dynamic route segments
- Generates route tree with lazy imports
- Generates route manifest with parent/child relationships

#### PledgeStack (`crates/adapter-pledgestack/`)
- React frontend + Rust backend framework adapter (like Next.js but backend in Rust)
- Scans `app/` for React `.tsx` pages (file-based routing, dynamic `[slug]` segments)
- Scans `server/api/` for Rust backend routes — recognizes both `.rs` and `.psx` files
- Scans `server/middleware/` for middleware files (`.rs` or `.psx`)
- Detects server entry point (`server/lib.rs`, `server/lib.psx`, `server/main.rs`, `server/main.psx`)
- Parses `#[route(GET, "/api/users")]` and `#[pledge::route(...)]` macros to extract HTTP method, path, handler
- Supports three macro formats: simple (`GET, "/path"`), qualified (`pledge::route(...)`), key-value (`method = "GET", path = "/path"`)
- Generates `RouteManifest` (JSON) with all frontend + backend routes + middleware
- `.psx` → `.rs` copy during build for `cargo build` compatibility
- SSR/SSG detection from `getServerSideProps` / `getStaticProps` / `revalidate` exports
- `.psx` extension: PledgeStack eXtension — brands backend files, parallel to `.tsx` for frontend

### CSS Processing (`transform/css.rs:transform_css` + `process_postcss`)
- Lightning CSS: minification, nesting, autoprefixing
- PostCSS pipeline: `@tailwind` directives, `@apply` expansion
- 80+ Tailwind utility class mappings
- CSS Modules: `*.module.css` scoped class names with blake3 content hashing (`generate_css_module_map`)

### Resolver (`crates/resolver/src/lib.rs`)
- The single resolution implementation — `pledgepack-core` depends on it and
  `BuildEngine::resolve` delegates to it via `engine::module_resolver`
  (aliases/extensions/conditions/workspace derived from `PledgeConfig`), so
  engine and tooling can no longer diverge
  (`crates/core/tests/resolver_conformance.rs` pins the contract)
- Resolution order: aliases (tsconfig + `resolve.alias`) → `imports`
  (`#subpath`) → relative → absolute → workspace packages → node_modules
- Package.json: `exports` (modern) → `module` → `main` → `browser`
- Exports conditions: `browser` > `import` > `module` > `require` > `default`
- Subpath patterns: `./utils/*` → `./utils/*.js`
- DashMap cache per (importer, specifier) pair; results are canonicalized

### Cache (`crates/cache/src/lib.rs`)
- `CacheKey`: blake3 hash of (content_hash, function_id, params)
- `CacheEntry`: { code, source_map, deps, created_at }
- Memory: `DashMap<CacheKey, CacheEntry>` — lock-free concurrent reads
- Disk: `bincode` serialization to `node_modules/.pledge-cache/`
- `FunctionCache::new(dir, persist)` — controls disk persistence

### Optimizer (`crates/optimizer/src/lib.rs`)
- **Reachability**: BFS from entry modules, mark all reachable
- **Side effects**: Heuristic detection (top-level assignments, console.log, etc.)
- **Chunk types**: Entry, Vendor (node_modules), Async (dynamic `import()`), Shared (2+ entries)
- **Export-demand shake**: reachable modules whose imports are all dead are recorded in a stub registry; emit writes `__pp.def` stubs for them so `__pp.req` never throws
- **Module wrapping**: emit lowers each module into `__pp.def`/`__pp.req` records in the `__pp` runtime — no raw ESM specifiers survive into output

### Dev Server (`crates/dev-server/src/lib.rs`)
- **Axum** router: `/` → index.html, `/__pledge_hmr` → WebSocket, `/__pledge_error` → error overlay, `/*` → module handler
- **On-demand transform**: Each HTTP request triggers full Oxc pipeline
- **AST-based import rewriting**: Oxc parser rewrites imports with string fallback (`./utils` → `./utils.js`)
- **Alias rewriting**: `@/components` → `/src/components` (resolve aliases)
- **Extension fallback**: `/src/utils.js` → resolves to `utils.ts` on disk
- **Import map injection**: Bare specifiers in `node_modules` resolved via import map in HTML
- **HTTPS support**: TLS via rustls + tokio-rustls (config: `https: { cert, key }`)
- **HMR**: `notify` crate watcher → debounce 150ms → WebSocket push to clients
- **CSS HMR**: CSS file changes send content via WebSocket, `<style>` tags updated in-place
- **Error overlay**: Transform errors sent via WebSocket with source context, file path, color-coded lines
- **Runtime error overlay**: `window.addEventListener('error')` and `window.addEventListener('unhandledrejection')` catch runtime browser errors and display them in the overlay with stack traces
- **Auto-open browser**: `open: true` config auto-opens default browser on dev server start via `opener` crate (cross-platform, handles WSL/sandboxed macOS)
- **Dev server proxy**: All HTTP methods (GET, POST, PUT, DELETE, PATCH) proxied via reqwest
- **WebSocket proxy**: `ws: true` on proxy config enables bidirectional WS proxying
- **Source maps**: `sourceMappingURL` comments appended to dev server responses
- **React Fast Refresh**: Component state preservation via `window.__pledge_fast_refresh`
- **React shim**: Minimal `React.createElement` injected in HTML (no React install needed)
- **Web Workers**: `new Worker(new URL(...))` patterns + `?worker`/`?sharedworker` import suffixes transformed for dev
- **Dynamic imports**: Oxc AST `ImportExpression` visitor for accurate detection
- **Network URL display**: Local network IP address shown alongside localhost URL via `local-ip-address` crate (e.g., `→ Network: http://192.168.x.x:3000`)

### WASM Plugin Host (`crates/wasm-plugin-host/src/lib.rs`)
- **Wasmtime 48.0.2** engine loads `.wasm` plugin files (WASM Component Model, WIT contract currently at v0.1.3)
- 10 hooks: `resolve-id`, `load`, `transform`, `transform-index-html`, `render-chunk`, `handle-hot-update`, `build-start`, `build-end`, `generate-bundle`, `configure-server`
- WIT contract: 11 record types, `cache-key` in every output for task graph caching
- Sandbox: restricted WASI context (no filesystem, no network, empty env)
- `WasmPluginHostBridge` — thread-safe wrapper (Mutex) for build engine integration
- All hooks wired into build pipeline via `wire_plugin_transform()`, `wire_plugin_resolve()`, `wire_plugin_load()`
- Plugin ordering: `enforce: "pre"|"post"` for both WASM and JS plugins
- Host imports: `get-config`, `emit-file`, `resolve-import` wired to engine resolver
- Plugin signing verification: `PluginSigningVerifier` validates plugin signatures (G12.35)
- Capability audit: `CapabilityAuditor` with approval/denial logic for plugin capabilities (G12.36)

### JS Plugin Host (`crates/js-plugin-host/src/lib.rs`)
- **Vite-compatible hooks**: `resolveId`, `load`, `transform`, `transformIndexHtml`, `configureServer`, `buildStart`, `buildEnd`, `generateBundle`
- **Embedded JS runtime**: **QuickJS** (rquickjs 0.12.2) — ES2020 compliant, 10-100x faster than Boa, ~500KB binary
- **Console support**: `console.log()` injected for plugin debugging
- **Plugin parsing**: Scans JS/TS source for hook definitions, evaluates source in JS context
- **Hook execution**: `transform()` hook calls JS function and parses JSON result
- **Build lifecycle**: `buildStart()` / `buildEnd()` / `generateBundle()` called during build
- **Content-addressed transform caching**: `transform_cache: HashMap<[u8; 32], TransformResult>` with blake3 keys
- **JsPluginHostBridge**: Thread-safe wrapper (dedicated thread, mpsc channels) for build engine integration

### Plugin System Security (`crates/core/src/plugin_system.rs`)
- **Rich error messages** (G12.16): `SourceError` with source context, line numbers, caret pointers, and `SuggestedFix` suggestions
- **Plugin signing** (G12.35): `PluginSignature` + `PluginSigningVerifier` — verifies plugin signatures before execution
- **Capability audit** (G12.36): `PluginCapability` + `CapabilityAuditor` — audits plugin capabilities with approval/denial logic

### Test Runner (`crates/js-plugin-host/src/test_runner.rs`)
- **Vitest-compatible API**: `describe`, `it`, `test`, `expect` with matchers (`toBe`, `toEqual`, `toBeTruthy`, `toContain`, `toHaveLength`, `toThrow`, `not` inverse matchers)
- **Lifecycle hooks**: `beforeAll`, `beforeEach`, `afterEach`, `afterAll`
- **Embedded JS runtime**: Tests run in **QuickJS** with `console.log` and `require()` shim
- **Test bundling** (`test_bundle.rs`): each test file is bundled with the project modules it imports — imports are resolved through the build resolver (aliases, tsconfig paths, `node_modules`, extensions), transformed by the same Oxc pipeline, and wrapped in a CommonJS-style registry. `vitest`/`@jest/globals` resolve to the harness globals; Node built-ins resolve to an empty object; unresolved imports throw only when executed. `test.setup_files` are bundled the same way.
- **Mock support**: `vi.fn()`, `vi.mock()`, `vi.spyOn()`, `vi.stubGlobal()` for Vitest-compatible mocking
- **Snapshot testing**: `toMatchSnapshot()` and `toMatchInlineSnapshot()` with `SnapshotStore` for `.snap` file persistence, auto-update mode, and mismatch error reporting
- **Coverage reporting**: `CoverageReport` with text, JSON, HTML, and LCOV output formats; line/function/branch coverage tracking
- **Test setup files**: `test.setup_files` config for running setup code before each test file
- **Test environments**: `test.environment` config — `node` (default), `jsdom` (DOM shims: document, window, navigator, location, customElements, MutationObserver, getComputedStyle), `happy-dom` (lighter DOM shims)
- **Globals mode**: `test.globals: true` to run tests with global `describe`, `it`, `test`, `expect` without imports
- **Test isolation**: `test.isolation` config — `file` (each file in own QuickJS context), `pool` (shared pool), `none` (no isolation)
- **UI mode**: `pledgepack test --ui` generates HTML report with pass/fail/skip summary, per-test status, error details, and serves it at `localhost:5174` with auto-browser-open
- **Config integration**: `run_test_file_with_config()` accepts `TestConfig` for full configuration support

### Environment Variables (`crates/core/src/env.rs`)
- **File loading**: `.env` → `.env.local` → `.env.[mode]` → `.env.[mode].local` (highest precedence last)
- **Variable expansion**: `${VAR}` syntax for referencing other env vars
- **Injection**: `import.meta.env.PLEDGE_*` replaced in code during transform
- **Built-in vars**: `PLEDGE_DEV`, `PLEDGE_PROD`, `PLEDGE_MODE`, `MODE`, `DEV`, `PROD`, `SSR`
- **Type generation**: `generate_dts()` produces `pledge-env.d.ts` with typed `ImportMetaEnv` interface

### HTML Processing (`crates/core/src/html.rs`)
- **Parsing**: Extracts `<script type="module">` src, `<link rel="stylesheet">`, `<link rel="modulepreload">`, `<title>`, `<meta>` tags
- **Production HTML**: Replaces script src with hashed filenames, injects CSS `<link>` tags
- **HTML minification**: `minify_html()` removes comments, collapses whitespace, strips redundant spaces
- **Default generation**: `generate_default_html()` creates `index.html` with entry script and title

### Source Maps (`crates/core/src/transform/utils.rs`)
- **V3 format**: Source maps with `sourcesContent` for debugging
- **Dev + production**: Generated in both modes

### Dependency Pre-Bundling (`crates/core/src/dep_bundler.rs`)

> Dev-server only — production builds resolve `node_modules` through the
> normal resolver + optimizer pipeline. `DepBundler::pre_bundle` runs at
> server startup.

- **Scanning**: Recursively scans source files for bare (non-relative) imports
- **CJS → ESM**: Generates ESM interop wrappers for CommonJS modules
- **Resolution**: Reads `package.json` `module`/`main` fields, prefers ESM
- **Output**: Pre-bundled deps written to `node_modules/.pledge-deps/`

### Parallel Transforms (`crates/core/src/engine.rs`)
- **Rayon**: `transform_modules_parallel()` uses `rayon::par_iter` for multi-core processing
- **Batch**: All modules transformed in parallel, errors propagated

### Compression (`crates/core/src/compression.rs`)
- **Gzip**: Real gzip compression via `flate2` — `.gz` files for JS, CSS, HTML, JSON, SVG, WASM
- **Brotli**: Real Brotli compression via `brotli` crate — `.br` files generated alongside gzip
- **Stats**: File count, original/compressed sizes, compression ratios

### Node.js Polyfills (`crates/core/src/polyfills.rs`)
- **20 built-in modules**: buffer, process, path, crypto, stream, util, events, url, os, fs, http, https, net, tls, zlib, querystring, string_decoder, timers, assert, child_process
- **Browser-safe**: Minimal ESM-compatible polyfills using Web APIs (Web Crypto, TextEncoder, fetch, etc.)
- **node: prefix**: Supports both `import 'path'` and `import 'node:path'` specifiers

### Define / Compile-Time Constants (`crates/core/src/transform/env.rs`)
- **Constant replacement**: Replace identifiers with literal values at build time
- **Type inference**: Automatically wraps strings, preserves numbers/booleans
- **Config**: `define: { 'process.env.NODE_ID': '"production"' }`

### import.meta.glob (`crates/core/src/transform/env.rs:expand_import_meta_glob`)
- **Glob-based file imports**: `import.meta.glob('./pages/*.tsx')` expanded at transform time
- **Lazy mode**: Default — returns object mapping paths to `() => import('./pages/Home.tsx')` dynamic import functions
- **Eager mode**: `{ eager: true }` — returns object mapping paths to directly imported modules
- **Query support**: `?raw` query returns file content as string, `import` filter for import-only
- **Recursive wildcards**: `**` for recursive directory matching (e.g., `./components/**/*.tsx`)
- **Path keys**: Object keys are the matched file paths relative to the importing module

### Library Mode (`crates/core/src/config.rs`)
- **Multiple formats**: ESM, CJS, UMD, IIFE output formats
- **External dependencies**: Mark packages as external (not bundled)
- **Type declarations**: Optional `.d.ts` generation

### Build Profiling (`crates/core/src/pipeline.rs`)
- **Per-phase timing**: Parse + Transform, Optimize, Emit phases timed individually
- **Total build time**: End-to-end build duration reported
- **Enable**: `pledgepack build --profile` or `profile: true` in config

### Edge Output (`crates/core/src/edge.rs`)
- **Cloudflare Workers**: Service Worker format with `fetch` handler + `wrangler.toml`
- **Vercel Edge**: Edge function format with `config.runtime = 'edge'` + `vercel.json`
- **Deno Deploy**: `Deno.serve()` format + `deno.json`

### Build Analyzer (`crates/core/src/analyzer.rs`)
- **Per-module**: Original + transformed sizes, dependencies, module kind
- **Chunks**: Modules grouped by directory with size summaries
- **Duplicates**: Same module name in different paths flagged
- **HTML report**: `pledgepack analyze` serves interactive HTML at `localhost:4200`

### Transform Optimizations (`crates/core/src/transform_optimizations.rs`)
- **WASM target compilation**: `?wasm` import suffix detects WASM modules and generates JS glue code
- **Tree shaking with side-effects**: `analyze_side_effects()` checks for global writes, DOM access, console calls
- **Cross-chunk variable hoisting**: `analyze_cross_chunk_hoisting()` tracks variables imported across chunks
- **CSS tree shaking**: `extract_used_class_names()` finds className/class/:class attributes including template literals; `shake_css()` filters unused CSS rules
- **Dead code elimination**: `eliminate_dead_code()` handles `if (false)`, `if (true)`, strict comparisons, typeof checks
- **Constant folding**: `fold_constants()` handles numeric, string, boolean, and typeof expression folding
- **Optional chaining optimization**: `optimize_optional_chaining()` simplifies redundant null checks
- **Module-level memoization**: `ModuleTransformCache` with blake3 content + config hash keys and LRU eviction

### CSS Features (`crates/core/src/css_features.rs`)
- **CSS `@layer` management**: `parse_layers()` detects and orders cascade layers
- **Container queries polyfill**: `polyfill_container_queries()` for older browser support
- **Critical CSS extraction**: `extract_critical_css()` finds above-the-fold selectors; `inline_critical_css()` inlines into HTML
- **CSS source maps**: `generate_css_source_map()` maps output to original `.scss`/`.less`/`.css`

### CSS-in-JS (`crates/core/src/css_in_js.rs`)
- **Compile-time extraction**: styled-components, emotion, vanilla-extract patterns
- **JS object to CSS**: `js_object_to_css()` converts JS style objects to CSS declarations
- **Template literal extraction**: Parses tagged template literals for CSS content

### Tailwind v4 (`crates/core/src/tailwind_v4.rs`)
- **Oxide engine integration**: `@theme`, `@utility`, `@variant` directive support
- **Theme detection**: `detect_tailwind_v4_theme()` identifies v4 config patterns
- **Utility generation**: Dynamic utility class generation from theme tokens

### Asset Pipeline (`crates/core/src/asset_pipeline.rs`)
- **MDX compilation**: `compile_mdx()` — Markdown + JSX with frontmatter extraction
- **GraphQL loading**: `parse_graphql()` + `graphql_to_module()` with TypeScript type generation
- **YAML/CSV/TSV imports**: Typed named exports from data files
- **Image format auto-selection**: `select_image_format()` + `generate_picture_element()` for WebP/AVIF
- **Audio/video assets**: URL exports with metadata
- **PDF assets**: Inline base64 support
- **Asset manifest**: `AssetManifest` with content-hashed output paths

### Plugin System (`crates/core/src/plugin_system.rs`)
- **Hot reload**: `PluginHotReloader` watches plugin files and reloads without restart
- **WASM sandboxing**: `SandboxLimits` (memory, CPU time) + `SandboxedFs` (filesystem access control)
- **Dependency resolution**: `PluginDependencyResolver` with import maps for npm packages in WASM sandbox
- **Lifecycle hooks**: `LifecycleHookRegistry` — `watchStart`, `watchChange`, `watchEnd`, before/after transform/build
- **Parallel execution**: `execute_parallel_transforms()` via rayon thread pool
- **Rich error messages** (G12.16): `SourceError` with source context, caret pointers, `SuggestedFix` suggestions
- **Plugin signing** (G12.35): `PluginSignature` + `PluginSigningVerifier` for signature verification
- **Capability audit** (G12.36): `PluginCapability` + `CapabilityAuditor` with approval/denial logic

### Ecosystem & Extensibility (`crates/core/src/ecosystem.rs`)
- **Plugin presets** (#94): `builtin_presets()` returns 6 built-in presets (react, tailwind, solid, vue, svelte, astro); `apply_presets()` merges plugin lists and config defaults; community presets via `pledgepack-preset-*` npm packages
- **Custom transformer pipeline** (#97): `build_pipeline()` constructs ordered transform steps with custom insertion; `TransformStep` tracks built-in vs custom steps; `replace_default` flag for full pipeline replacement
- **Workspace-aware resolution** (#98): `detect_workspace()` auto-detects npm/pnpm/yarn workspaces from package.json, pnpm-workspace.yaml, or lerna.json; `resolve_workspace_import()` resolves bare specifiers to local packages via exports/module/main fields
- **Cross-package HMR** (#99): `HmrDependencyMap` maps files↔packages; `build_hmr_map()` scans workspace source files; `compute_hmr_set()` computes HMR propagation set including reverse dependencies
- **Shared build cache** (#100): `resolve_shared_cache_dir()` returns workspace root `.pledge/cache/` for cross-package cache sharing

### Output Distribution (`crates/core/src/output_distribution.rs`)
- **Performance budgets**: `check_budget()` enforces per-entry and per-chunk size limits
- **Bundle size diff**: `diff_snapshots()` + `format_diff_report()` with regression detection
- **Source map explorer**: `build_source_map_tree()` + `generate_explorer_html()` with interactive treemap
- **Multi-format output**: `generate_multi_format()` — ESM, CJS, IIFE, UMD for library mode

### Service Worker (`crates/core/src/service_worker.rs`)
- **Service worker generation**: Precaching strategies (cache-first, network-first, stale-while-revalidate)
- **Per-route caching** (#113): `sw: { caching: [{ pattern: '/api/*', strategy: 'network-first' }] }` config generates `sw.js`
- **Web App Manifest**: `generate_manifest()` produces manifest.json with icons, theme, display mode

### Advanced Features (`crates/core/src/advanced.rs`)
- **Web Components** (#110): `.wc.tsx`/`.wc.jsx` → Custom Elements with Shadow DOM, `customElements.define()` registration
- **Module Federation** (#115): Host bootstrap + remote entry generation, shared modules with singleton/eager/requiredVersion
- **GraphQL Code Generation** (#116): TypeScript types + React hooks from `.graphql` schema files via `--codegen`
- **Environment-Specific Builds** (#117): `--env staging` loads `.env.staging`, injects `process.env.*` defines
- **Post-Build Hooks** (#118): Sitemap generation, HTML meta tag injection (viewport, description, charset)
- **Conditional Exports** (#119): Custom `package.json` exports conditions via `exports: { conditions: [...] }` config
- **Build Concurrency** (#120): `build: { parallel: N }` config, auto-detects CPU cores, capped at 16

### LSP Server (`crates/core/src/lsp_server.rs`)
- **Import resolution**: `extract_import_path()` parses import/require statements
- **Go-to-definition**: Resolves module specifiers to file paths
- **Diagnostics**: Real-time error reporting with `DiagnosticSeverity`
- **Hover info**: Type and documentation on hover
- **Document symbols**: `SymbolKind` enumeration for outline view

### Migration Tooling (`crates/core/src/migrate.rs`)
- **Config migration**: `migrate_config()` from Vite/webpack/CRA/Next.js to `pledge.config.ts`
- **Static evaluation**: source configs are parsed with Oxc and folded to JSON (`js_config.rs`) — never executed. Values that need a runtime (`path.resolve`, function calls) are kept as `$call`/`$expr` markers and produce explicit warnings in the migration output.
- **Field mapping**: entry, `base`, `define`/`DefinePlugin`, aliases, `server.port`/`open`/`https`/`proxy`, `build.outDir`/`sourcemaps`, `HtmlWebpackPlugin`/`MiniCssExtractPlugin` → html/CSS extraction equivalents; `render_config_ts()` renders the result as `pledge.config.ts`
- **Framework detection**: framework plugins in the source config (e.g. `@vitejs/plugin-react`) set the `framework` field
- **Dry run**: `--dry-run` flag shows what would be migrated without writing files

### Incremental Build Graph (`crates/core/src/module_graph.rs`)
- **Content-hash change detection**: Only rebuild changed modules and transitive dependents
- **Persistent serialization**: `SerializableModuleGraph` saves/loads via bincode to `module_graph.bin`

### Remote Cache (`crates/cache/src/remote.rs`)
- **S3/GCS/HTTP backends**: `RemoteCache` with automatic fallback
- **3-tier cache**: Memory → disk → remote, integrated in `BuildEngine`

### Git Cache Invalidation (`crates/cache/src/git_cache.rs`)
- **Git tree hashes**: `GitCacheInvalidator` uses `git ls-files` and `git rev-parse HEAD^{tree}`
- **Faster invalidation**: Tree hash comparison instead of per-file content hashing

### Dev Server Optimizations (`crates/dev-server/src/`)
- **Native file watcher** (`watcher.rs`): Platform-specific inotify/FSEvents/ReadDirectoryChangesW
- **HMR partial updates** (`hmr_diff.rs`): Line-level diff via `similar` crate (Myers algorithm) pushed through WebSocket
- **Cold boot optimization** (`lazy_pipeline.rs`): Deferred Oxc/Lightning CSS initialization
- **Middleware chain** (`middleware.rs`): Configurable request processing pipeline
- **fs_guard / origin_guard**: dev-server file-serving security — deny-lists, symlink and 8.3-shortname tricks, and Origin header checks against DNS-rebinding

### Observability & Monitoring (#101–#105)
- **Build telemetry dashboard** (`crates/core/src/telemetry.rs`): `pledgepack dashboard` command serves interactive web UI at `localhost:4300` with build history chart, cache hit rate, module counts, and build durations. Build records persisted to `.pledge/history.json` (max 100 entries).
- **OpenTelemetry/OTLP export** (G12.26): `OtlpExporter` exports build spans via OTLP (gRPC/HTTP) to collectors like Jaeger, Zipkin, or Grafana. `TaskSpan` records with deterministic FNV-1a span IDs. `OtlpExportConfig` with endpoint, protocol, headers, timeout.
- **Bundle size budget CI** (`crates/core/src/budgets.rs`): `pledgepack build --check-budgets` flag verifies total bundle size, per-chunk size, chunk count, and per-entry budgets. Exits non-zero on violations. Emits GitHub Actions `::error` annotations when `GITHUB_ACTIONS` env is set. Generates PR comment markdown with chunk size table.
- **Performance regression detection** (`crates/core/src/bench.rs`): `pledgepack bench --baseline <ref>` compares median build time against stored baseline. `--threshold` flag sets regression percentage (default 10%). Baseline results persisted in `.pledge/bench.json` keyed by git ref.
- **Module dependency graph** (`crates/core/src/analyzer.rs`): `pledgepack analyze --graph` generates interactive force-directed graph HTML with canvas-based physics simulation. Circular dependencies detected via DFS and highlighted in red. Legend distinguishes entry, CSS, module, and circular nodes.
- **Build event webhooks** (`crates/core/src/webhooks.rs`): `webhooks: { onBuild: URL, onError: URL }` config sends POST requests after builds. Auto-detects Slack and Discord webhook URL formats and generates appropriate message payloads. Custom headers supported via `webhooks.headers`.

### Internationalization & Accessibility (#106–#109)
- **i18n-aware bundling** (`crates/core/src/i18n.rs`): `i18n: { locales: [...], defaultLocale: 'en', messagePattern: './messages.${locale}.json' }` config enables locale-based bundle splitting. Transforms `${locale}` import patterns into runtime locale detection shims. Only the current locale's strings are loaded at runtime.
- **RTL CSS auto-generation** (`crates/core/src/rtl.rs`): `css: { rtl: 'auto' }` config auto-generates RTL CSS from LTR stylesheets using CSS logical properties. Converts `margin-left` → `margin-inline-start`, `padding-right` → `padding-inline-end`, `text-align: left` → `text-align: start`, and 20+ other physical-to-logical mappings. Generated as `[dir="rtl"]` scoped CSS files alongside LTR output.
- **Accessibility linting** (`crates/core/src/a11y.rs`): `a11y: { enabled: true, failOnError: true }` config checks HTML output for missing `alt` attributes on images, missing ARIA labels on interactive elements, insufficient color contrast, missing `<html lang>`, missing `<title>`, and form inputs without labels. Exits non-zero when `failOnError` is true and errors are found.
- **Build-time string encryption** (`crates/core/src/encrypt.rs`): `encrypt: { keys: ['API_KEY'], key: '<hex>' }` config encrypts sensitive string values at build time using XOR cipher with base64 encoding. Injects a runtime `__pledge_decrypt()` shim that decrypts values at runtime. Prevents plain-text secrets from appearing in bundle output.

### Advanced CSS, Security & Performance (#66–#84)
- **Advanced CSS** (`crates/core/src/css_advanced.rs`): CSS Modules `composes` directive parsing and cross-file resolution (#66). Automatic dark mode CSS generation from `prefers-color-scheme` or custom property inversion (#67). CSS custom property optimization — inline static vars, remove unused, minify names in production (#68). Scoped CSS for React with `data-v-xxxxx` attribute selectors (#69). CSS nesting polyfill verification via lightningcss (#70).
- **Performance** (`crates/core/src/performance.rs`): Route-based chunk splitting — `detect_routes()` scans app/pages, `split_by_routes()` in optimizer creates per-route chunks with shared extraction (#71). Module prefetch directives — `generate_prefetch_tags()` for `<link rel="modulepreload">` and `<link rel="prefetch">` (#72). CSS-in-JS runtime tree shaking — `strip_css_in_js_runtime()` removes styled-components/emotion/vanilla-extract runtime imports after static extraction (#73). WASM streaming compilation — `generate_wasm_streaming_code()` outputs `WebAssembly.instantiateStreaming()` with fallback (#74). Precompute module hash at transform time — `TransformOutput.content_hash` field (#75).
- **Security** (`crates/core/src/security.rs`): Subresource Integrity (SRI) — `inject_sri_into_html()` generates SHA-384 `integrity` attributes for script/link tags (#81). Content Security Policy — `CspGenerator` analyzes HTML and generates `_headers` file with CSP (#82). Dependency vulnerability scanning — `scan_vulnerabilities()` checks package.json against CVE database, integrated in `pledgepack doctor` (#83). License compliance — `scan_licenses()` reads node_modules package.json license fields, `check_license_compliance()` validates against whitelist/blacklist, integrated in `pledgepack doctor` (#84).

### Determinism & Verification (`crates/core/src/determinism.rs`)
- **Provenance tracking** (G11.6): `ProvenanceRecord` tracks input sources, environment, and tool versions for each build artifact. Enables reproducible builds by recording exact inputs.
- **Determinism lockfile** (G11.7): `DeterminismLockfile` serializes provenance records to a lockfile for build reproducibility verification across machines.
- **Formal verification** (G11.8): `CreusotVerificationConfig` for cargo-creusot integration — formal verification of task determinism properties.

### Task System Advanced Features (`crates/task-system/`, `crates/task-system-macros/`)
- **Zig-generated input hashing hot path** (G2.14): `#[task]` macro generates SIMD-accelerated hashing path using Zig native layer for 128-bit blake3 task IDs.
- **Generic task support** (G2.15): `#[task]` macro supports generic functions — type parameters included in TaskId computation via `type_to_string_generic()`.
- **Trait method tasks** (G2.16): `#[task]` macro supports trait method implementations — `self`/`Receiver` parameters handled correctly, excluded from task inputs.
- **Custom environment plugins** (G5.12): `EnvironmentPlugin` and `EnvironmentPluginRegistry` in `crates/task-system/src/environment.rs` — pluggable environment detection for task-aware builds.
- **Asset pipeline enhancements**: Font subsetting already in `fonts.rs`, wired via `build.font_subsetting` (#76). SVG sprite `?sprite` suffix in `transform_asset()` (#77). Video poster frame extraction in `transform_video_asset()` — exports `poster` URL (#78). Responsive image srcset via `config.image.responsive_widths` (#79). Asset inlining threshold via `build.assets_inline_limit` (#80).

---

# Build System

## `pledgepack build` Pipeline

```
1. Load config (pledge.config.ts → pledge.config.js → pledge.config.mjs → pledge.json → defaults)
2. Create BuildEngine with config
3. BuildEngine::build()
   ├── BFS from entry point (src/index.tsx)
   ├── For each module:
   │   ├── Resolve specifier → file path (Resolver)
   │   ├── Read file content (Zig read_file via C ABI)
   │   ├── Compute content hash (blake3)
   │   ├── Check memory cache (HashMap<u64, CachedOutput>)
   │   ├── If miss → check disk cache (FunctionCache/bincode)
   │   ├── If miss → transform:
   │   │   ├── Oxc Parser → AST
   │   │   ├── Oxc SemanticBuilder → symbols + scopes
   │   │   ├── Oxc Transformer → strip types, JSX → React.createElement
   │   │   ├── Oxc Codegen → JavaScript string
   │   │   ├── Env injection (import.meta.env.PLEDGE_* replacement)
   │   │   └── Source map generation (V3 with sourcesContent)
   │   ├── Store result in memory cache + disk cache
   │   ├── Extract imports → resolve → add to graph → enqueue
   │   └── Track stats (built vs cached)
   └── Return BuildResult { modules_built, modules_cached, duration_ms }
4. Run Optimizer
   ├── Tree shake (remove unreachable modules)
   ├── Split chunks (entry / vendor / shared)
   └── Return Vec<Chunk>
5. Emit to .pledge/
   ├── Lower each module's ESM into `__pp.def`/`__pp.req` records and wrap
   │   chunks in the `__pp` runtime (crates/core/src/bundle.rs) — dynamic
   │   `import()` resolves via `__pp.dyn` + `__pp_manifest.js`
   ├── Write each chunk as a content-hashed .js file
   ├── Generate index.html (with hashed asset references)
   ├── Generate manifest.json (chunk-id keyed entry-to-file mapping)
   ├── Generate source maps (per-chunk indexed maps merged from module maps)
   └── Scan emitted output for secrets (security.secretScan, on by default —
       recognized credentials fail the build)
6. Post-build steps:
   ├── Generate pledge-env.d.ts (if env_dts enabled)
   ├── Process HTML entry point
   ├── Load JS plugins (buildStart hooks)
   ├── Generate edge bundle (if edge_target configured)
   ├── Generate service worker (if configured)
   ├── Generate Web App Manifest (if configured)
   ├── Check performance budgets (if configured)
   ├── Generate bundle size diff (if previous snapshot exists)
   ├── Multi-format output (if library mode configured — ESM/CJS/IIFE/UMD)
   ├── Record build telemetry (#101) — save to .pledge/history.json
   ├── Check bundle size budgets (#102) — if --check-budgets flag or budgets.enabled
   ├── Lint HTML for accessibility (#108) — if a11y.enabled
   ├── Send build event webhooks (#105) — if webhooks.onBuild/onError configured
   ├── Inject SRI hashes (#81) — if security.sri enabled
   ├── Generate CSP _headers file (#82) — if security.csp == "auto"
   └── Compress output (gzip .gz + brotli .br files)
```

## Transform Optimizations Pipeline

During step 3 (transform), the following optimizations are applied in order:

```
1. CSS tree shaking — extract_used_class_names() from JS/JSX/TSX, shake_css() removes unused selectors
2. Dead code elimination — eliminate_dead_code() removes unreachable branches (if false, if true)
3. Constant folding — fold_constants() evaluates compile-time expressions (1 + 2 → 3)
4. Optional chaining optimization — optimize_optional_chaining() simplifies redundant null checks
5. Cross-chunk variable hoisting — analyze_cross_chunk_hoisting() prepares shared variable declarations
6. Module-level memoization — ModuleTransformCache checks content + config hash before re-transforming
7. WASM target compilation — ?wasm import suffix generates JS glue code for WASM modules
8. i18n import transform (#106) — ${locale} patterns replaced with runtime locale detection shims
9. String encryption (#109) — sensitive string values encrypted with XOR + base64, runtime decrypt shim injected
```

## CSS Processing Pipeline

```
1. Lightning CSS — minification, nesting, autoprefixing
2. CSS Modules — scoped class names with blake3 content hashing
3. PostCSS / Tailwind — @tailwind directives, @apply expansion
4. Tailwind v4 — @theme, @utility, @variant directive processing
5. CSS-in-JS extraction — styled-components, emotion, vanilla-extract compile-time extraction
6. CSS @layer — cascade layer management and ordering
7. Container queries — polyfill for older browsers
8. Critical CSS — extract_critical_css() + inline_critical_css() for faster FCP
9. CSS source maps — generate_css_source_map() maps to original source
10. PostCSS plugin caching — blake3 content hash for incremental processing
11. RTL CSS auto-generation (#107) — if css.rtl is 'auto' or 'manual', generates [dir="rtl"] scoped CSS from LTR output using logical property mappings
12. Dark mode CSS (#67) — generate_dark_mode_css() auto-generates dark variants from prefers-color-scheme or custom property inversion
13. Custom property optimization (#68) — optimize_custom_properties() inlines static vars, removes unused, minifies names (production)
14. Scoped CSS for React (#69) — scope_css_with_attribute() adds data-v-xxxxx attribute selectors
15. CSS Modules composes (#66) — parse_composes() + strip_composes() for cross-file composition resolution
16. CSS-in-JS runtime tree shaking (#73) — strip_css_in_js_runtime() removes runtime imports after static extraction
```

## Asset Pipeline

```
MDX files        → compile_mdx() — Markdown + JSX with frontmatter
GraphQL files    → parse_graphql() + graphql_to_module() with TypeScript types
YAML/CSV/TSV     → transform_yaml() / transform_csv() / transform_tsv() with typed exports
Images           → select_image_format() — WebP/AVIF auto-selection, generate_picture_element()
                   Responsive srcset (#79) — config.image.responsive_widths for custom breakpoints
                   Asset inlining (#80) — assets under build.assets_inline_limit auto-inlined as base64
Audio/Video      → transform_audio_asset() / transform_video_asset() with URL exports
                   Video poster (#78) — transform_video_asset() exports poster URL alongside src
PDF              → transform_pdf_asset() with inline base64 support
SVG              → optimize_svg() + ?sprite suffix (#77) generates <symbol> sprite sheet
Fonts            → optimize_fonts() (#76) with FontSubsetConfig, wired via build.font_subsetting
WASM             → transform_wasm() (#74) with WebAssembly.instantiateStreaming() + SIMD auto-detection
All assets       → AssetManifest with content-hashed output paths
```

## Plugin System

```
1. Plugin discovery — scan configured plugin paths
2. Plugin loading — WASM plugins via wasmtime 48.0.2, JS plugins via QuickJS (rquickjs 0.12.2)
3. Hot reload — PluginHotReloader watches for file changes, reloads without restart
4. Sandboxing — SandboxLimits (memory, CPU time) + SandboxedFs (filesystem access)
5. Dependency resolution — PluginDependencyResolver with import maps for npm packages
6. Lifecycle hooks — LifecycleHookRegistry:
   ├── watchStart / watchChange / watchEnd (dev mode)
   ├── beforeTransform / afterTransform
   └── beforeBuild / afterBuild
7. Parallel execution — execute_parallel_transforms() via rayon thread pool
```

## WIT Plugin Contract — Design Decisions

> Contract at v0.1.3 (additive from v0.1.0: `render-chunk` in v0.1.2, `handle-hot-update` in v0.1.3) · WASM validation complete

The plugin ABI is the **one-way door**. Once plugins are written against this contract, breaking it nukes the ecosystem.

### Decision 1: WASM Component Model as First-Class, JS Shim as Second-Class

Two-tier plugin system. Pure WASM on day one starves the ecosystem — nobody writes WASM plugins for a bundler with no users. Pure JS kills the moat — you're Rolldown with worse adoption. Two tiers gives ecosystem access (JS shim) + a real moat (WASM path) + an upgrade ladder (JS → WASM for speed + cache).

Both tiers implement the same hook shapes. The JS shim translates Vite/Rollup plugin output to the WIT contract's types. A plugin author can start with JS (second-class) and port to WASM (first-class) without changing their hook logic.

### Decision 2: Mirror Vite/Rollup Hook Shapes

The WIT hooks mirror `resolveId`, `load`, `transform`, `transformIndexHtml`, `buildStart`, `buildEnd`, `generateBundle`, `configureServer`. The JS shim must be a thin wrapper — if the WIT hooks differ significantly, the shim becomes a translation layer with semantic mismatches.

**Intentional differences:**
1. No `config`/`configResolved` hooks — config is resolved before plugins load
2. `configure-server` returns middleware source instead of receiving a server object — sandbox-friendly
3. `cache-key` in every output — Vite/Rollup don't have this; it's the PledgePack moat

### Decision 3: Cache Key in Every Hook Output

Every hook output includes `cache-key: string` (blake3 hash of inputs). This is the core of the moat — every hook output is a cacheable task graph node.
- **WASM plugins:** Fine-grained caching. The plugin knows its inputs and computes a precise cache key.
- **JS shim plugins:** Coarse caching. The shim computes a coarse key (input hash + plugin path).

Same hash used by `TaskId` (128-bit blake3). Hex-encoded (64 chars).

### Decision 4: AST Access via Optional `ast-json` Field

The `transform` hook input includes `ast-json: option<string>`, always `none` in v0.1.0. Forward compatible — when AST serialization is implemented, plugins that declare `needs-ast: true` will receive the AST. Plugins must handle `none` gracefully.

### Decision 5: v0.1.0 Freeze, Additive-Only Changes

v0.1.0 is frozen. Additive changes (new hooks, new optional fields) allowed in v0.1.x. Breaking changes require v1.

**Additive:** New hooks, new optional fields (`option<T>`), new flags in `hook-flags`.
**Breaking (requires v1):** Removing/renaming a hook, changing a field type, changing hook semantics, removing a field.

### Decision 6: No `config`/`configResolved` Hooks in v0.1.0

Plugins cannot read or modify PledgePack config in v0.1.0. ✅ **Implemented in v0.1.1** — `get-config` host import added. Plugins can call `get-config()` to read resolved config JSON. Also added `emit-file()` and `resolve-import()` host imports.

### Decision 7: `configure-server` Returns Middleware Source

Instead of receiving a server object to mutate, `configure-server` returns `server-middleware` (source code). The WASM sandbox boundary prevents passing object references. The JS shim can pass the server object directly and translates the middleware registration into the return type.

### Decision 8: Lifecycle Hooks Are Parallel

`build-start`, `build-end`, `generate-bundle` are parallel — all plugins are called, order is not guaranteed. Faster and avoids ordering bugs. Vite calls these sequentially; PledgePack calls them in parallel.

### Hook Semantics Summary

| Hook | Semantics | Chain? | Cache |
|---|---|---|---|
| `resolve-id` | First non-null wins | No | Per-plugin, keyed by (source, importer, kind) |
| `load` | First non-null wins | No | Per-plugin, keyed by id |
| `transform` | Chain (each sees previous output) | Yes | Per-plugin, keyed by (code, id, ast-json) |
| `transform-index-html` | Chain | Yes | Per-plugin, keyed by (html, path) |
| `build-start` | All called (parallel) | No | Not cached (side effects only) |
| `build-end` | All called (parallel) | No | Not cached (side effects only) |
| `generate-bundle` | All called (parallel) | No | Not cached (side effects only) |
| `configure-server` | All called (parallel) | No | Not cached (dev mode only) |

### Resolved Open Questions

1. **Plugin ordering:** ✅ **Implemented in v0.1.1** — `enforce: option<string>` in `plugin-metadata`. Both JS and WASM plugins support `enforce: "pre"|"post"`.
2. **Host imports:** ✅ **Implemented in v0.1.1** — `get-config`, `emit-file`, `resolve-import` added for both WASM (wasmtime linker) and JS (`pledgepack` global).
3. **Error handling:** Host catches the trap, logs it, treats the hook as returning `none`.
4. **Resource limits:** Configurable in pledgepack.config, default 128MB memory, no CPU limit.
5. **Plugin discovery:** `plugins: ["./my-plugin.wasm"]` in config, same as JS plugins.

## Oxc Transform Details

### Source Type Detection
```rust
SourceType::from_path(path) →
  .tsx → SourceType::tsx()
  .ts  → SourceType::ts()
  .jsx → SourceType::jsx()
  .js  → SourceType::mjs()
```

### Transform Options
```rust
TransformOptions {
  jsx: {
    // Framework-aware:
    //   React → JsxRuntime::Classic (React.createElement, no react/jsx-runtime import)
    //   Solid → JsxRuntime::Automatic, import_source = "solid-js"
    //   Vue   → JsxRuntime::Automatic, import_source = "vue"
    runtime: JsxRuntime::Classic,
    development: false,
  },
  typescript: {
    only_remove_type_imports: false,
  },
}
```

### Classic vs Automatic JSX Runtime
- **Classic**: `React.createElement("div", null, "hello")` — requires `React` global
- **Automatic**: `import { jsx } from "react/jsx-runtime"` — requires React installed
- **Solid**: `import { createComponent } from "solid-js"` — automatic runtime with solid-js
- **Vue**: `import { jsx } from "vue"` — automatic runtime with vue
- Pledge defaults to **Classic** for React to avoid requiring React installation for simple projects

### Framework-Specific Transforms

#### Vue SFC (`.vue`)
- Extracts `<template>`, `<script setup>`, `<style scoped>` blocks via `extract_sfc_block()`
- Template compiled to render function
- Scoped CSS: `[data-v-pledge]` attribute selectors injected
- Output: JS module with render function + extracted CSS

#### Svelte (`.svelte`)
- Extracts `<script>`, `<style>`, and markup blocks
- Markup compiled to DOM render function with mount/unmount
- Scoped CSS: `[svelte-pledge]` attribute selectors
- Output: JS module with render function + extracted CSS

#### Astro (`.astro`)
- Parses `---` frontmatter delimiters
- Template compiled to async render function
- `<style>` blocks extracted as CSS
- Output: JS module with async render function + extracted CSS

### PostCSS / Tailwind Processing
- `@tailwind base` → Tailwind CSS reset (box-sizing, margins, borders)
- `@tailwind components` → `.container` responsive class
- `@tailwind utilities` → Full utility CSS subset (display, flex, spacing, typography, etc.)
- `@apply` expansion → 80+ utility class mappings
- Processed before Lightning CSS parsing

### Web Workers (#111, #112)
- `new Worker(new URL('./worker.ts', import.meta.url))` → `new Worker('/src/worker.js')`
- `new SharedWorker(new URL(...))` patterns also supported
- `.worker.js` / `.worker.ts` extensions detected as `ModuleKind::Worker`
- `?worker` import suffix: `import MyWorker from './worker.ts?worker'` → `const MyWorker = function() { return new Worker('/src/worker.js'); }`
- `?sharedworker` import suffix: `import MyWorker from './worker.ts?sharedworker'` → `const MyWorker = function() { return new SharedWorker('/src/worker.js'); }`
- Resolver strips `?worker`/`?sharedworker` suffixes during module resolution
- `ModuleKind::SharedWorker` for shared worker modules

### Web Components (#110)
- `.wc.tsx` / `.wc.jsx` files compiled to Custom Elements via `compile_web_component()`
- Automatic `customElements.define('tag-name', ClassName)` registration
- Shadow DOM with `mode: 'open'` — CSS scoped inside shadow root
- Component name extracted from `export function` / `export const` declarations
- Tag name auto-generated as kebab-case from component name
- `ModuleKind::WebComponent` for web component modules

### Service Worker Caching (#113)
- Per-route caching strategy configuration via `sw: { caching: [...] }` config
- Strategies: `cache-first`, `network-first`, `stale-while-revalidate`, `network-only`, `cache-only`
- `sw: { cache_name: 'pledge-sw', offline_fallback: '/offline.html' }` config
- Generates `sw.js` in output directory during build

### Module Federation (#115)
- `federation: { name: 'host', remotes: { app1: 'http://cdn/app1.js' }, shared: ['react'] }` config
- Host bootstrap: `__pledge_federation__` global with remotes/shared/init/loadRemote
- Remote entry: `__pledge_remote__` with exposes/shared/get/init
- Shared modules support `requiredVersion`, `singleton`, `eager` options
- `parse_federation_config()` parses JSON config into `FederationConfig` struct

### GraphQL Code Generation (#116)
- `pledgepack build --codegen` generates TypeScript types from `.graphql` schema files
- `graphql: { schema: 'schema.graphql', output: 'src/generated', react_hooks: true }` config
- Generates TypeScript interfaces for all GraphQL types
- Nullable fields typed as `T | null`, lists as `T[]`
- React hooks generated for Query operations (`useXxx` pattern)
- Output: `src/generated/graphql-types.ts`

### Environment-Specific Builds (#117)
- `pledgepack build --env staging` loads `.env.staging` file
- Env vars injected as `process.env.*` defines at build time
- `NODE_ENV` resolved from env file or production flag
- Multiple environments without code changes: `.env.development`, `.env.staging`, `.env.production`

### Post-Build Optimization Hooks (#118)
- `run_post_build_hooks()` executes after build emit
- Generates `sitemap.xml` from HTML chunks
- Injects missing HTML meta tags: viewport, description, charset
- `PostBuildContext` provides output dir, HTML path, chunks, assets
- `PostBuildResult` reports sitemap generation, HTML modification, warnings

### Conditional Exports Resolution (#119)
- `exports: { conditions: ['production', 'browser'] }` config
- Custom conditions checked first, then defaults: browser > import > module > require > default
- Supports sugar form (`{ "import": "...", "require": "..." }`) and subpath patterns (`"./components/*"`)
- Wildcard `*` pattern matching in export keys
- `Resolver::with_conditions()` constructor for custom conditions

### Build Concurrency Control (#120)
- `build: { parallel: 4 }` config limits concurrent module transforms
- Auto-detects CPU cores via `std::thread::available_parallelism()` when not configured
- Capped at 16 threads maximum
- Uses dedicated rayon thread pool via `ThreadPoolBuilder::install()`
- Prevents OOM on large projects with many modules

### Dynamic Import Detection
- Oxc AST `ImportExpression` visitor for accurate detection
- String-based fallback if parsing fails
- Only relative specifiers (`./`, `../`) tracked for chunk splitting
- Stored in `TransformOutput.dynamic_imports` for optimizer use

### React Fast Refresh (Dev Mode)
- AST-based component detection using Oxc (function declarations, arrow functions with capitalized names)
- Injects `import.meta.hot.accept()` with component registration
- Component state preserved via `window.__pledge_fast_refresh` registry
- Only injected in development mode for React framework

### Define / Compile-Time Constants
- Replace identifiers with literal values at build time via `apply_define()`
- Config: `define: { 'process.env.NODE_ID': '"production"', '__VERSION__': '"1.0.0"' }`
- Type inference: strings wrapped in quotes, numbers/booleans preserved

### import.meta.glob
- Glob-based file imports for dynamic route/component discovery
- `import.meta.glob('./pages/*.tsx')` expanded at transform time via `expand_import_meta_glob()`
- **Lazy mode** (default): Returns `{ './pages/Home.tsx': () => import('./pages/Home.tsx'), ... }`
- **Eager mode**: `{ eager: true }` returns `{ './pages/Home.tsx': moduleObject, ... }`
- **Query support**: `?raw` returns file content as string, `import` filter for import-only
- **Recursive**: `**` wildcard for recursive directory matching (e.g., `./components/**/*.tsx`)
- Replaced at transform time in the JS transform pipeline after env variable replacement

### Node.js Polyfills
- 20 built-in module polyfills available when `node_polyfills: true` in config
- Supports both `import 'path'` and `import 'node:path'` specifiers
- Browser-safe ESM polyfills using Web APIs (Web Crypto, TextEncoder, fetch, etc.)
- Modules: buffer, process, path, crypto, stream, util, events, url, os, fs, http, https, net, tls, zlib, querystring, string_decoder, timers, assert, child_process

## Caching

### Two-Tier Architecture
```
Request → Memory Cache (HashMap)
              Hit? → return cached output
              Miss? → Disk Cache (bincode)
                        Hit? → load into memory, return
                        Miss? → Transform
                                  → Store in memory + disk
```

### Cache Key
```rust
CacheKey = blake3(content_hash || function_id || params)
```
- `content_hash`: u64 hash of file source content
- `function_id`: "transform" (currently single function)
- `params`: file path string

### Cache Location
- Default: `node_modules/.pledge-cache/`
- Configurable via `pledge.config.ts`: `{ cache: { dir: '.pledge-cache', enabled: true } }`

### Cache Invalidation
- Content-based: File change → new content hash → cache miss → retransform
- Manual: `pledgepack cache clear` removes all disk cache files
- Automatic: Old entries are not garbage collected (future: TTL-based eviction)

## Production Output (`.pledge/`)

### File Structure
```
.pledge/
├── index.html            # Generated HTML shell (with hashed asset references)
├── entry-0.<hash>.js     # Wrapped chunk: __pp runtime + __pp.def module records
├── entry-0.<hash>.js.map # Per-chunk source map (module maps merged into sections)
├── __pp_manifest.js      # Runtime manifest: module key → async chunk URL
├── manifest.json         # Chunk-id → output file mapping
└── ...                   # Vendor/shared/async chunks, CSS files, assets
```

### Compression Output
When `compress_gzip` and/or `compress_brotli` are enabled in config:
```
.pledge/
├── index.html.gz           # Gzip compressed (flate2)
├── index.html.br           # Brotli compressed (brotli crate)
├── entry-0.<hash>.js.gz    # Gzip compressed chunk
├── entry-0.<hash>.js.br    # Brotli compressed chunk
└── ...
```
Compressible file types: `.js`, `.mjs`, `.css`, `.html`, `.json`, `.svg`, `.wasm`

### HTML Minification
- `minify_html()` removes HTML comments, collapses whitespace, strips redundant spaces between tags
- Applied during production builds for smaller HTML output

### Build Profiling
- Per-phase timing: Parse + Transform, Optimize, Emit phases timed individually
- Enable with `pledgepack build --profile` or `profile: true` in config
- Reports timing for each phase and total build duration

### Edge-Ready Output
When `edge_target` is configured, generates edge-function-compatible bundles:

| Target | Output File | Format |
|--------|-------------|--------|
| `cloudflare` | `worker.js` + `wrangler.toml` | Service Worker with `fetch` handler |
| `vercel` | `edge.js` + `vercel.json` | Edge function with `config.runtime = 'edge'` |
| `deno` | `mod.ts` + `deno.json` | `Deno.serve()` format |

### HTML Generation
The HTML processor (`crates/core/src/html.rs`) parses `index.html` as an entry point:
- Extracts `<script type="module">` src paths as entry points
- Extracts `<link rel="stylesheet">` and `<link rel="modulepreload">` hrefs
- Extracts `<title>` and `<meta>` tags
- In production: replaces script src with hashed filenames, injects CSS `<link>` tags
- HTML minification: `minify_html()` removes comments, collapses whitespace
- If no `index.html` exists, generates a default one with `generate_default_html()`

### Extension Mapping
- `.tsx` → `.js`
- `.ts` → `.js`
- `.jsx` → `.js`
- `.js` → `.js` (passthrough after transform)
- `.wc.tsx` / `.wc.jsx` → `.js` (Web Component compiled, Custom Element registered)
- `.vue` → `.js` (SFC compiled, CSS extracted)
- `.svelte` → `.js` (SFC compiled, CSS extracted)
- `.astro` → `.js` (compiled, CSS extracted)
- `.css` → `.css` (Lightning CSS processed)
- `.json` → `.js` (named + default exports)
- `.wasm` → `.js` (async instantiation wrapper)
- `.graphql` / `.gql` → `.js` (with `--codegen`: TypeScript types generated)
- `.png`/`.jpg`/`.svg`/etc. → URL string export (or base64 if `?inline`)

### Asset Hashing
- Content hash (blake3) appended to filenames: `logo-a1b2c3d4.png`, `entry-0.4d2e7cdd.js`
- `manifest.json` generated mapping chunk ids to hashed output paths
- Enables long-term browser caching with cache busting

### Library Mode
- `LibraryConfig` with ESM, CJS, UMD, IIFE output formats
- External dependencies: mark packages as external (not bundled)
- Optional `.d.ts` type declarations generation
- Config: `library: { entry, formats, name, external, declarations }`

### Single-File Bundle
- `emit_single_file()` routes through the same wrapped-chunk emitter with a
  single entry chunk containing every module
- All imports inlined (no external chunk files) — `import()` targets resolve
  locally via `__pp.req` instead of loading a chunk

## Optimizer

### Tree Shaking
1. Start from entry module IDs
2. BFS through dependency graph
3. Mark all reachable modules
4. Unreachable modules are excluded from chunks

### Chunk Splitting
```
Entry chunks:  Entry module + exclusive dependencies
Vendor chunk:  All modules in node_modules/
Shared chunk:  Modules used by 2+ entry points
Route chunks:  Per-route modules (#71) — split_by_routes() extracts shared route modules
```
- **Route-based splitting (#71)**: `detect_routes()` scans app/pages directories, `split_by_routes()` creates per-route chunks with a shared chunk for modules used across routes
- **Module prefetch (#72)**: `generate_prefetch_tags()` creates `<link rel="modulepreload">` and `<link rel="prefetch">` based on route chunks and prefetch strategy

### Module Wrapping
- Each module's ESM is lowered into a `__pp.def(key, fn)` factory record
  (`crates/core/src/bundle.rs`) — imports become `__pp.req(key)` calls and
  exports install getters on the module's exports object
- Tree-shaken modules register empty factories so `__pp.req` resolves to
  `{}` instead of throwing
- `import()` of a same-chunk target lowers to
  `Promise.resolve().then(() => __pp.req(key))`; cross-chunk targets go
  through `__pp.dyn(key)` + `__pp_manifest.js`

## Parallel Transforms

The engine supports parallel module transforms using rayon:
```rust
engine.transform_modules_parallel(modules: Vec<(ModuleId, ResolvedModule)>)
```
- Uses `rayon::par_iter` for multi-core processing
- All modules transformed in parallel
- Errors propagated (first error stops collection)
- Falls back to sequential if single module

## Dependency Pre-Bundling (dev server)

The dep bundler (`crates/core/src/dep_bundler.rs`) is used by the **dev
server** (bare imports resolve through an import map to `.pledge-deps`
URLs) — it no longer runs in the production build path, which bundles
`node_modules` deps through the normal chunk emit. It pre-bundles bare
imports:
1. Scans source files for bare (non-relative) import specifiers
2. Resolves each from `node_modules` via `package.json` `module`/`main` fields
3. Converts CJS modules to ESM with interop wrappers
4. Writes pre-bundled output to `node_modules/.pledge-deps/`

CJS → ESM interop wrapper:
```javascript
const __pledge_cjs_module = {};
const module = { exports: __pledge_cjs_module };
// ... original CJS code ...
export default module.exports;
```

## Environment Variable Injection

The env module (`crates/core/src/env.rs`) loads `.env` files and injects variables:

### File Loading Order (highest precedence last)
1. `.env`
2. `.env.local`
3. `.env.[mode]` (e.g., `.env.production`)
4. `.env.[mode].local` (e.g., `.env.production.local`)

### Variable Expansion
```bash
PLEDGE_API_URL=http://localhost:8080
PLEDGE_FULL_URL=${PLEDGE_API_URL}/api/v1
```

### Code Injection
`import.meta.env.PLEDGE_*` references in source code are replaced with actual values during transform.

### Type Generation
`pledgepack generate-env-types` generates `pledge-env.d.ts`:
```typescript
interface ImportMetaEnv {
  readonly PLEDGE_API_URL: string;
  readonly PLEDGE_DEV: boolean;
  // ...
}
interface ImportMeta {
  readonly env: ImportMetaEnv;
}
```

## Test Runner (`crates/js-plugin-host/src/test_runner.rs`)

### Overview
The built-in test runner provides a Vitest-compatible testing experience using the **QuickJS** (rquickjs 0.12.2) embedded JS runtime. Tests are run without external dependencies (no Node.js, Jest, or Vitest required).

### Configuration
In `pledge.config.ts`:
```typescript
export default defineConfig({
  test: {
    include: ['**/*.{test,spec}.{ts,tsx,js,jsx}'],
    exclude: ['node_modules', '.pledge', 'dist'],
    environment: 'node', // 'node' | 'jsdom' | 'happy-dom'
    globals: false, // Global describe/it/expect without imports
    setup_files: [], // e.g. ['./test/setup.ts']
    isolation: 'file', // 'file' | 'pool' | 'none'
    coverage: false, // Enable coverage collection
    coverage_reporter: 'text', // 'text' | 'json' | 'html' | 'lcov'
    snapshot: true, // Enable snapshot testing
    snapshot_dir: '__snapshots__',
    update_snapshots: false, // Auto-update snapshots
  },
});
```

### API Support
- **Test functions**: `describe`, `it`, `test`, `it.skip`, `test.skip`, `it.only`, `test.only`
- **Assertions**: `expect()` with `toBe`, `toEqual`, `toBeTruthy`, `toBeFalsy`, `toBeNull`, `toBeUndefined`, `toBeDefined`, `toContain`, `toHaveLength`, `toThrow`, and `not` inverse matchers
- **Lifecycle hooks**: `beforeAll`, `beforeEach`, `afterEach`, `afterAll`
- **Mocking**: `vi.fn()`, `vi.mock()`, `vi.spyOn()`, `vi.stubGlobal()`
- **Snapshot testing**: `toMatchSnapshot()`, `toMatchInlineSnapshot()` with `.snap` file persistence

### Test Environments
| Environment | Description |
|-------------|-------------|
| `node` (default) | No DOM shims, minimal `process` and `Buffer` stubs |
| `jsdom` | Full DOM shim: `document`, `window`, `navigator`, `location`, `customElements`, `MutationObserver`, `getComputedStyle`, `HTMLElement` |
| `happy-dom` | Lighter DOM shim: `document`, `window`, `navigator`, `location`, `customElements`, `MutationObserver` |

### Test Isolation
| Mode | Description |
|------|-------------|
| `file` (default) | Each test file runs in its own QuickJS JS context |
| `pool` | Shared pool of contexts for batch execution |
| `none` | No isolation — all tests share a single context |

### Coverage Reporting
- **Coverage tracking**: Line, function, and branch coverage via `__pledge_coverage` global
- **Report formats**: `text` (console output), `json` (machine-readable), `html` (styled report), `lcov` (for CI integration)
- **Config**: `test.coverage: true` to enable, `test.coverage_reporter` to select format

### UI Mode
- `pledgepack test --ui` generates an HTML report with:
  - Pass/fail/skip summary with colored indicators
  - Per-test file breakdown with suite and test names
  - Error messages and stack traces for failed tests
  - Execution duration per test
- Report served at `localhost:5174` with auto-browser-open
- Report also written to `.pledge/test-report.html`

### Snapshot Testing
- **`toMatchSnapshot()`**: Serializes value to JSON, compares against stored `.snap` file
- **`toMatchInlineSnapshot()`**: Compares against inline snapshot string
- **Auto-update**: `test.update_snapshots: true` or `-u` flag updates stale snapshots
- **Storage**: `.snap` files stored in `test.snapshot_dir` (default: `__snapshots__`)
- **Mismatch reporting**: Detailed diff shown on snapshot mismatch

## Observability & Monitoring (#101–#105)

### Build Telemetry Dashboard (#101)

`pledgepack dashboard` serves an interactive web UI at `localhost:4300` showing build history:

```
.pledge/history.json — persistent build records (max 100 entries)
```

Each build record includes:
- Timestamp, duration (ms), success/failure status
- Modules built vs cached, cache hit rate
- Bundle size (bytes)
- Error message (if failed)

The dashboard renders an SVG chart with build duration trend, cache hit rate, and a summary table of recent builds.

### Bundle Size Budget CI (#102)

`pledgepack build --check-budgets` or `budgets: { enabled: true }` in config:

```typescript
export default defineConfig({
  budgets: {
    enabled: true,
    maxBundleSize: 500_000,   // 500KB total
    maxChunkSize: 250_000,    // 250KB per chunk
    maxChunkCount: 10,        // max 10 chunks
    entryBudgets: {           // per-entry overrides
      'src/index.tsx': 200_000,
    },
  },
});
```

**CI integration**: When `GITHUB_ACTIONS` env is set, violations are emitted as `::error` annotations:
```
::error file=.pledge/src/index.js::Bundle size budget exceeded: 320KB > 250KB
```

### Performance Regression Detection (#103)

`pledgepack bench --baseline <ref> --threshold <pct>`:

```
pledgepack bench --baseline main --threshold 10
```

- Runs 5 build iterations, takes median duration
- Compares against stored baseline in `.pledge/bench.json`
- Exits non-zero if regression exceeds threshold (default: 10%)
- Use `pledgepack bench --save-baseline <ref>` to store a new baseline

### Module Dependency Graph (#104)

`pledgepack analyze --graph` generates an interactive force-directed dependency graph:

- Canvas-based physics simulation (Verlet integration)
- Nodes color-coded by type: entry (green), CSS (blue), module (gray), circular (red)
- Edges represent import relationships
- Circular dependencies detected via DFS and highlighted
- Served at `localhost:4200`

### Build Event Webhooks (#105)

```typescript
export default defineConfig({
  webhooks: {
    enabled: true,
    onBuild: 'https://hooks.slack.com/services/...',
    onError: 'https://discord.com/api/webhooks/...',
    headers: { 'Authorization': 'Bearer token' },
  },
});
```

- Auto-detects Slack vs Discord from URL format
- Slack: formatted as attachment with color-coded status, fields for duration/modules/bundle size
- Discord: formatted as embed with color, title, description, and fields
- Sent asynchronously after build completion

## Internationalization & Accessibility (#106–#109)

### i18n-Aware Bundling (#106)

```typescript
export default defineConfig({
  i18n: {
    enabled: true,
    locales: ['en', 'fr', 'ar'],
    defaultLocale: 'en',
    messagePattern: './locales/${locale}.json',
  },
});
```

- Import patterns containing `${locale}` are transformed at build time
- Only the default locale's messages are bundled; other locales loaded via dynamic import
- Runtime shim detects `document.documentElement.lang` or `navigator.language`

### RTL CSS Auto-Generation (#107)

```typescript
export default defineConfig({
  css: {
    rtl: 'auto',  // 'auto' | 'manual' | 'off'
  },
});
```

When enabled, for each CSS file emitted, a corresponding `.rtl.css` file is generated:

| LTR Property | RTL Property |
|---|---|
| `margin-left` | `margin-inline-start` |
| `margin-right` | `margin-inline-end` |
| `padding-left` | `padding-inline-start` |
| `padding-right` | `padding-inline-end` |
| `text-align: left` | `text-align: start` |
| `text-align: right` | `text-align: end` |
| `left: 10px` | `inset-inline-start: 10px` |
| `right: 10px` | `inset-inline-end: 10px` |
| `border-left` | `border-inline-start` |
| ... 20+ mappings | |

RTL output is scoped with `[dir="rtl"]` selector.

### Accessibility Linting (#108)

```typescript
export default defineConfig({
  a11y: {
    enabled: true,
    failOnError: true,
    checkAlt: true,
    checkAria: true,
    checkContrast: false,
  },
});
```

Checks performed on HTML output:
- **img-alt**: `<img>` tags missing `alt` attribute
- **button-aria-label**: Interactive `<button>` without text content or `aria-label`
- **input-label**: `<input>` without associated `<label>` or `aria-label`
- **html-lang**: `<html>` missing `lang` attribute
- **html-title**: Document missing `<title>` element
- **color-contrast**: Insufficient contrast ratios (optional)

### Build-Time String Encryption (#109)

```typescript
export default defineConfig({
  encrypt: {
    enabled: true,
    key: 'a1b2c3d4e5f6...',  // hex-encoded XOR key
    keys: ['API_KEY', 'SECRET_TOKEN'],  // variable names to encrypt
  },
});
```

- Scans code for string literals assigned to configured variable names
- Encrypts values using XOR cipher with base64 encoding
- Injects `__pledge_decrypt()` runtime shim in bundle output
- Encrypted values appear as `__pledge_decrypt("base64string")` in output
- Prevents plain-text secrets from appearing in bundle source

## JSON Schema Generation (`pledgepack schema`)

Generates a JSON Schema for the `pledge.config.ts` configuration, enabling IDE autocompletion and validation:

```bash
pledgepack schema              # Output to stdout
pledgepack schema --output schema.json  # Write to file
```

- Uses `schemars` crate to derive `JsonSchema` from `PledgeConfig` and all sub-structs/enums
- Schema covers all config fields: build, dev_server, cache, plugins, image, i18n, a11y, encrypt, etc.
- Can be used with VS Code JSON validation, JetBrains schema support, or any JSON Schema consumer

## PledgeStack Framework Adapter

> **Boundary:** PledgePack is the bundler layer; PledgeStack is the framework
> layer. PledgePack handles route discovery and manifest generation — it does
> NOT handle SSR rendering, API route execution, middleware execution, or
> `.psx` transpilation. See [CONNECTION.md](./CONNECTION.md) for the full
> responsibility split.

PledgeStack is a Next.js-like full-stack framework with React frontend and Rust backend:

```
my-app/
├── app/                        # Frontend routes (React .tsx)
│   ├── page.tsx                # → GET /
│   ├── about/page.tsx          # → GET /about
│   └── blog/[slug]/page.tsx    # → GET /blog/:slug
├── server/                     # Backend (Rust .rs or .psx)
│   ├── api/
│   │   ├── users.rs            # → /api/users
│   │   └── auth.psx            # → /api/auth
│   ├── middleware/
│   │   └── auth.rs
│   └── lib.rs                  # Server entry point
├── components/                 # Co-located React components
├── lib/                        # Shared utilities
├── types/                      # TypeScript types
├── pledge.config.ts
├── Cargo.toml                  # Rust deps for server/
└── package.json                # JS deps for frontend
```

### Route Discovery
- `PledgeStackAdapter::discover()` scans `app/` and `server/` directories
- Frontend routes: `page.tsx` files in `app/` directory
- Backend routes: `#[route(GET, "/api/users")]` macros in `.rs` or `.psx` files
- Middleware: files in `server/middleware/`
- Server entry: `server/lib.rs`, `server/lib.psx`, `server/main.rs`, or `server/main.psx`

### `.psx` Extension
- PledgeStack eXtension — brands backend files, parallel to `.tsx` for frontend
- Treated as Rust source code (same syntax, same tooling with VS Code file association)
- Copied to `.rs` during build for `cargo build` compatibility

### Route Macro Formats
```rust
// Simple
#[route(GET, "/api/users")]
pub async fn list_users() { }

// Qualified
#[pledge::route(POST, "/api/auth")]
pub async fn login() { }

// Key-value
#[route(method = "DELETE", path = "/api/users/:id")]
pub async fn delete_user() { }
```

## Plugin Presets (#94)

Plugin presets bundle plugins and config defaults for specific ecosystems.

```ts
// pledge.config.ts
export default defineConfig({
  presets: ['react', 'tailwind'],  // applies both presets
})
```

**Built-in presets**: `react`, `tailwind`, `solid`, `vue`, `svelte`, `astro`

**Community presets**: Install `pledgepack-preset-{name}` from npm. The preset's `preset.json` is loaded automatically.

```json
// node_modules/pledgepack-preset-remix/preset.json
{
  "name": "remix",
  "plugins": ["./plugins/remix-router.js"],
  "config_defaults": { "framework": "react" },
  "description": "Remix preset with SSR routing"
}
```

## Custom Transformer Pipeline (#97)

Insert custom transform steps at any point in the pipeline.

```ts
export default defineConfig({
  transform_pipeline: {
    pipeline: ['my-wasm-transform', 'minify'],
    replace_default: false,  // insert into default pipeline
  }
})
```

When `replace_default: false` (default), custom steps are inserted after `oxc` and before `minify`:
```
oxc → [custom steps] → minify → tree-shake
```

When `replace_default: true`, only the configured steps run:
```
oxc → my-wasm-transform → minify
```

## Monorepo & Workspaces (#98–#100)

### Workspace-Aware Resolution (#98)

Auto-detects npm/pnpm/yarn workspaces and resolves local packages instead of hitting node_modules.

```ts
export default defineConfig({
  workspaces: {
    enabled: true,           // auto-detect workspace root
    root: './',              // explicit root (optional)
    cross_package_hmr: true, // #99
    shared_cache: true,      // #100
  }
})
```

Detection walks up the directory tree looking for:
- `package.json` with `"workspaces"` field (npm/yarn)
- `pnpm-workspace.yaml` (pnpm)
- `lerna.json` (lerna monorepos)

Bare specifiers like `@myorg/ui` resolve to the local workspace package, not the npm registry.

### Cross-Package HMR (#99)

When a file in a workspace package changes, HMR propagates to all consuming packages. The `HmrDependencyMap` scans source files, detects cross-package imports, and computes the full HMR update set.

### Shared Build Cache (#100)

When `shared_cache: true`, the build cache is placed at `{workspace_root}/.pledge/cache/` instead of per-package `node_modules/.pledge-cache/`. This enables cache reuse across packages for faster incremental builds.


---

# Dev Server & HMR

## `pledgepack dev` — Development Server

### Overview
The dev server serves source files from `src/` with on-demand Oxc transforms. Unlike `pledgepack build` which pre-builds everything to `.pledge/`, the dev server transforms each file when requested by the browser.

### Routes

| Route | Handler | Description |
|-------|---------|-------------|
| `GET /` | `index_handler` | Serves HTML shell with React shim + HMR client + error overlay |
| `GET /__pledge_hmr` | `hmr_websocket_handler` | WebSocket endpoint for HMR updates |
| `GET /__pledge_error` | `error_overlay_handler` | Error overlay endpoint |
| `GET /*path` | `module_handler` | Transforms and serves any source file |
| `GET /api/*` | `proxy_handler` | Proxied API requests (if proxy configured) |

### On-Demand Transform Flow

```
Browser requests /src/index.tsx
        │
        ▼
   module_handler
   ├── Resolve file on disk (with extension fallback)
   │   └── /src/utils.js → tries .tsx, .ts, .jsx, .mjs, .json
   ├── Read file content (Zig read_file)
   ├── Determine ModuleKind from extension
   ├── Oxc Transform:
   │   ├── Parse (SourceType from extension)
   │   ├── Semantic analysis (symbols + scopes)
   │   ├── Transform (JSX → React.createElement, TS → strip types)
   │   └── Codegen (JavaScript output)
   ├── Rewrite imports: "./utils" → "./utils.js"
   ├── Inject HMR boundary: import.meta.hot.accept()
   └── Return with Content-Type: application/javascript
```

### Import Rewriting

The browser's ES module loader requires file extensions in import specifiers. Pledge uses Oxc AST-based rewriting with string fallback:

| Source import | Rewritten to |
|---------------|-------------|
| `import { foo } from "./utils"` | `import { foo } from "./utils.js"` |
| `import { bar } from "../helpers"` | `import { bar } from "../helpers.js"` |
| `import { x } from "@/components"` | `import { x } from "/src/components"` (alias rewriting) |
| `import React from "react"` | Resolved via import map (dep pre-bundling) |
| `import('./lazy')` | `import('./lazy.js')` (dynamic import rewriting) |

### Extension Fallback

When the browser requests `/src/utils.js` (because import rewriting added `.js`), the dev server:
1. Checks if `utils.js` exists on disk → no
2. Tries `utils.tsx` → no
3. Tries `utils.ts` → yes! Serves transformed `utils.ts`

Order: `.tsx` → `.ts` → `.jsx` → `.mjs` → `.json`

Also supports: `.vue` → `.svelte` → `.astro` → `.css` → `.worker.js` → `.module.css`

### Web Worker Support

The dev server transforms `new Worker(new URL('./worker.ts', import.meta.url))` patterns:
- Rewrites to `new Worker('/src/worker.js')` for browser compatibility
- `new SharedWorker(new URL(...))` patterns also supported
- `.worker.js` / `.worker.ts` extensions detected as worker modules
- Worker scripts served with same on-demand transform pipeline

### Dynamic Import Support

`import('./lazy')` specifiers are detected via Oxc AST `ImportExpression` visitor:
- Relative specifiers get `.js` extension: `import('./lazy')` → `import('./lazy.js')`
- String-based fallback if parsing fails
- Dynamic imports collected in `TransformOutput.dynamic_imports` for future code splitting
- Async chunks can be loaded on-demand by the browser

### HTML Shell

The dev server generates HTML with:
- `<div id="root">` mount point
- Inline React shim (`React.createElement` minimal implementation)
- `<script type="module" src="/src/index.tsx">` entry point
- WebSocket HMR client script
- **Error overlay**: Full-screen overlay with error message, file path, source context, and color-coded line numbers
- **CSS HMR**: `updatePledgeCSS()` and `fetchPledgeCSS()` functions for injecting/updating `<style>` tags
- **Auto-reconnect**: WebSocket reconnects with exponential backoff on disconnect

### React Shim

Since React is not installed, a minimal `React.createElement` is injected:
- Creates DOM elements for string types
- Handles `children`, `className`, `style`, `onClick` props
- Supports `React.Fragment` (renders as `<div>`)
- No virtual DOM diffing (direct DOM manipulation)

## HMR (Hot Module Replacement)

### Architecture

```
File saved on disk
        │
        ▼
   notify crate watcher (recursive, project root)
        │
        ▼
   Debounce 150ms (batch rapid changes)
        │
        ▼
   Broadcast WebSocket message to all clients
   {
     "type": "update",
     "path": "/src/index.tsx"
   }
        │
        ▼
   Client-side: reloads the changed script tag
   with ?t=timestamp cache buster
```

### File Watcher
- **Crate**: `notify` (cross-platform file system notifications)
- **Scope**: Recursive watch on project root
- **Debounce**: 150ms to batch rapid saves (e.g., format-on-save + content change)
- **Filter**: Only triggers on `.ts`, `.tsx`, `.js`, `.jsx`, `.css`, `.json` files

### WebSocket Protocol
- **Endpoint**: `ws://localhost:3000/__pledge_hmr`
- **Connection**: Client connects on page load
- **Messages**:
  - `{ "type": "connected", "message": "Pledge HMR connected" }` — on connect
  - `{ "type": "update", "path": "/src/index.tsx" }` — on JS/TS file change
  - `{ "type": "update", "path": "/src/style.css", "css": "..." }` — on CSS file change (with content)
  - `{ "type": "error", "message": "...", "file": "...", "source": "..." }` — on transform error
  - `{ "type": "server-reload", "path": "...", "message": "Server code changed — reloading..." }` — on server-only file change
  - `{ "type": "server-reload-complete", "path": "...", "message": "Server code reloaded successfully" }` — after server reload completes

### Client-Side HMR
```javascript
const ws = new WebSocket('ws://' + location.host + '/__pledge_hmr');
ws.onmessage = (event) => {
    const data = JSON.parse(event.data);
    if (data.type === 'update') {
        if (data.css) {
            // CSS HMR: update <style> tag in-place
            updatePledgeCSS(data.path, data.css);
        } else {
            // JS HMR: reload the changed script
            const links = document.querySelectorAll('script[src="' + data.path + '"]');
            links.forEach(link => {
                const newLink = document.createElement('script');
                newLink.type = 'module';
                newLink.src = data.path + '?t=' + Date.now();
                link.replaceWith(newLink);
            });
        }
    } else if (data.type === 'error') {
        // Show error overlay with source context
        showPledgeError(data.message, data.file, data.source);
    }
};
```

### HMR Boundary Injection

For `.ts`, `.tsx`, `.js`, `.jsx` files, the dev server appends:
```javascript
// HMR boundary
if (import.meta.hot) {
    import.meta.hot.accept();
}
```

This allows modules to accept updates without full page reloads.

### React Fast Refresh

For React components in development mode, the dev server injects Fast Refresh code:

1. **Component detection**: Scans for function declarations and arrow functions with capitalized names
2. **Registration**: Components registered in `window.__pledge_fast_refresh` registry
3. **State preservation**: On HMR update, component state is preserved via React's `useState` hooks
4. **Boundary injection**: `import.meta.hot.accept()` with component list for targeted updates

```javascript
// Injected by Pledge for React Fast Refresh
if (import.meta.hot) {
    import.meta.hot.accept();
    window.__pledge_fast_refresh = window.__pledge_fast_refresh || function(name, reload) {
        (window.__pledge_fast_refresh_registry = window.__pledge_fast_refresh_registry || {})[name] = reload;
    };
    window.__pledge_fast_refresh[import.meta.url] = ['App', 'Header'];
}
```

### Framework-Specific Dev Support

| Framework | Dev Features |
|-----------|-------------|
| **React** | Fast Refresh, AST-based component detection, classic JSX shim |
| **Solid** | Automatic JSX runtime, `development: true` mode, dedicated adapter crate |
| **Vue** | SFC parsing, scoped CSS, render function compilation |
| **Svelte** | SFC parsing, scoped CSS, DOM render functions |
| **Astro** | Frontmatter parsing, async render, style extraction |

## Error Overlay

The dev server includes an in-browser error overlay for transform errors and runtime errors:

### Transform Errors
- **Source context**: Shows the error line with surrounding code (5 lines before/after)
- **Color-coded**: Line numbers in gray, error line highlighted in red
- **File path**: Full file path displayed at top of overlay
- **Auto-clear**: Overlay disappears when the next successful HMR update arrives
- **WebSocket delivery**: Errors pushed to all connected clients in real-time

### Runtime Errors
- **window.error events**: Catches uncaught JavaScript errors via `window.addEventListener('error')`
- **Unhandled promise rejections**: Catches via `window.addEventListener('unhandledrejection')`
- **Stack traces**: Runtime error stack traces displayed in the overlay
- **Auto-clear**: Overlay dismisses on next successful HMR update

### Error Message Format
```json
{
  "type": "error",
  "message": "Unexpected token in expression: '...'",
  "file": "/src/index.tsx",
  "source": "line 1\nline 2\nline 3 (error)\nline 4\nline 5"
}
```

## Auto-Open Browser

The dev server can automatically open the default browser when it starts:

### Configuration
In `pledge.config.ts`:
```typescript
export default defineConfig({
  dev_server: {
    open: true, // Auto-open browser on dev server start
  },
});
```

Or via CLI flag:
```bash
pledgepack dev --open
```

### Implementation
- Uses the `opener` crate for cross-platform browser opening
- Handles WSL, sandboxed macOS, and Linux variants automatically
- No platform-specific code needed — single `opener::open(url)` call

## CSS HMR

CSS file changes are handled without full page reloads:

### How It Works
1. File watcher detects CSS file change
2. CSS content is read and included in the HMR WebSocket message
3. Client-side `updatePledgeCSS(path, css)` function:
   - Finds existing `<style data-pledge-path="...">` tag
   - If found: replaces its `textContent` with new CSS
   - If not found: creates a new `<style>` tag and appends to `<head>`
4. No page reload needed — styles update instantly

## Server-Only Hot Reload

When `server_entry` is configured in `pledge.config.ts`, the dev server detects changes to server-only files and triggers a graceful reload while preserving WebSocket connections to connected clients.

### Configuration

In `pledge.config.ts`:
```typescript
export default defineConfig({
  server_entry: 'server/index.ts', // Path to your server entry point
  dev_server: {
    hmr: true,
  },
});
```

### How It Works

1. **Server directory detection**: `compute_server_dirs()` derives server-only directories from the `server_entry` path (e.g., `server/index.ts` → `server/`). Common SSR/API directories (`api/`, `server/`, `src/api/`, `src/server/`, `app/api/`) are also checked.

2. **File classification**: `is_server_file()` checks if a changed file is the server entry file itself or resides in a server-only directory.

3. **HMR update sequence**:
   - `server-reload` message sent to all connected clients with a "Server code changed — reloading..." message
   - Brief 100ms delay to let clients process the notification
   - `server-reload-complete` message sent to signal the server is back

4. **Client-side UI**: A banner appears at the top of the page showing "⟳ Server reloading..." and disappears when the reload completes. WebSocket connections are preserved throughout the reload.

### Client-Side Handler
```javascript
if (data.type === 'server-reload') {
    showPledgeServerReload(data.message); // Shows banner
} else if (data.type === 'server-reload-complete') {
    clearPledgeServerReload(); // Removes banner
}
```

## HTTPS Support

The dev server supports HTTPS via rustls + tokio-rustls:

### Configuration
In `pledge.config.ts`:
```typescript
export default defineConfig({
  https: {
    cert: './cert.pem',
    key: './key.pem',
  },
  dev_server: {
    port: 3000,
  },
});
```

When HTTPS is configured, the dev server serves over TLS, enabling testing of Secure Context APIs (Service Workers, Web Crypto, etc.).

## Import Map Injection

Bare specifiers (e.g., `import React from 'react'`) are resolved via import maps:
- Dep pre-bundler scans source files for bare imports
- Resolves from `node_modules` via `package.json` `exports`/`module`/`main` fields
- Generates import map injected into HTML `<script type="importmap">`
- Pre-bundled deps written to `node_modules/.pledge-deps/`

## Dev Server Proxy

The dev server can proxy API requests to a backend server:

### Configuration
In `pledge.config.ts`:
```typescript
export default defineConfig({
  dev_server: {
    proxy: [
      {
        path: '/api',
        target: 'http://localhost:8080',
        rewrite: true, // Remove /api prefix when forwarding
        ws: true, // Enable WebSocket proxy
        headers: { 'X-Forwarded-Host': 'localhost:3000' }
      }
    ]
  }
});
```

### How It Works
- Requests matching a proxy `path` prefix are forwarded to the `target` URL
- All HTTP methods supported: GET, POST, PUT, DELETE, PATCH
- If `rewrite` is true, the path prefix is stripped (e.g., `/api/users` → `http://localhost:8080/users`)
- If `rewrite` is false, the full path is preserved (e.g., `/api/users` → `http://localhost:8080/api/users`)
- Uses `reqwest` for HTTP forwarding
- Hop-by-hop headers are stripped from the proxy response

### WebSocket Proxy
- Set `ws: true` on proxy config to enable bidirectional WebSocket proxying
- Uses `tokio-tungstenite` for WS bridge between client and target
- Useful for HMR or live-reload backends

## Source Maps in Dev

The dev server appends `sourceMappingURL` comments to transformed modules:
```
//# sourceMappingURL=data:application/json;charset=utf-8;base64,...
```
This enables browser DevTools to show original source code instead of transformed output.

## `pledgepack serve` — Production Server

Simple static file server for `.pledge/`:
- **Crate**: `axum` + `tower-http::ServeDir`
- **Port**: 4000 (configurable)
- **Purpose**: Preview production build locally
- **No transforms**: Serves pre-built files as-is

```bash
pledgepack build   # Build to .pledge/
pledgepack serve   # Serve .pledge/ on :4000
```

## Dev Server Optimizations (Features 9-15)

### Native File Watcher (`crates/dev-server/src/watcher.rs`)
- Platform-specific native watchers for lower latency:
  - **Linux**: `inotify` via `notify` crate
  - **macOS**: `FSEvents` via `notify` crate
  - **Windows**: `ReadDirectoryChangesW` via `notify` crate
- Fallback to polling watcher if native APIs unavailable
- 200ms debounce to batch rapid file changes
- Filters out `node_modules`, `.pledge`, `target`, `.git` directories

### HMR Partial Updates (`crates/dev-server/src/hmr_diff.rs`)
- **Line-level diff**: Uses `similar` crate (Myers algorithm) to compute minimal diff between old and new module content
- **No line limit**: Previous 200-line LCS cap removed — `similar` handles any file size efficiently
- **`is_small()` heuristic**: Only sends diff for small changes, falls back to full replacement for large changes
- **WebSocket transport**: Diff sent via WebSocket as JSON `{ type: "diff", path, additions, deletions }`
- **Reduced bandwidth**: Only changed lines transmitted instead of full module

### Cold Boot Optimization (`crates/dev-server/src/lazy_pipeline.rs`)
- **Deferred initialization**: Oxc parser and Lightning CSS only initialized on first request
- **Dirty dependency tracking**: Only re-transforms modules whose dependencies changed
- **Lazy pipeline**: Transform pipeline components loaded on-demand

### WebSocket Compression
- `tower-http` `CompressionLayer` with gzip and `Fastest` quality level
- Per-message deflate for HMR WebSocket to reduce bandwidth on large module updates

### Multi-Entry Dev Server
- `detect_entries()` auto-detects HTML files in project root
- Each HTML entry gets independent HMR context
- Per-entry routes registered dynamically

### Middleware Chain (`crates/dev-server/src/middleware.rs`)
- Configurable middleware pipeline for request processing
- `MiddlewareFn` parsed from config (auth, logging, headers, CORS, rewrites)
- Middleware executed before module serving
- CORS and rewrite helpers built-in

### On-Demand Dependency Optimization
- Import patterns tracked per-module in `DevServerState`
- Re-optimizes dependencies only when import patterns change
- Not on every server start — faster cold boots

## Network URL Display

The dev server displays the local network URL alongside localhost, so you can test on other devices:

```
  → Local:    http://localhost:3000
  → Network:  http://192.168.1.42:3000
```

- Uses `local-ip-address` crate to detect the machine's network IP
- Shown for both HTTP and HTTPS dev servers
- Useful for testing on mobile devices, other machines, or VMs on the same network

## `pledgepack dashboard` — Build Telemetry (#101)

The dashboard command serves an interactive web UI for build observability:

```
pledgepack dashboard [--port 4300]
```

- Serves at `localhost:4300` (configurable via `--port`)
- Reads build history from `.pledge/history.json` (populated during `pledgepack build`)
- Displays SVG chart with build duration trends and cache hit rates
- Shows recent build summary table with status, duration, module counts
- No build history required to run — shows empty state if no builds recorded

### Build History Records
Each `pledgepack build` records telemetry data:
```json
{
  "timestamp": "2024-01-15T10:30:00Z",
  "duration_ms": 1234,
  "success": true,
  "modules_built": 42,
  "modules_cached": 18,
  "cache_hit_rate": 0.3,
  "bundle_size": 245678
}
```
