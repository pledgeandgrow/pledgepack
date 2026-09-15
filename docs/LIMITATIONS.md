# PledgePack Limitations

Known limitations, trade-offs, and areas for improvement.

> **2026-09-15 audit note:** several "✅ Resolved" entries below were found to
> overstate the real state of the code — see
> [`PRODUCTION-READINESS-100.md`](PRODUCTION-READINESS-100.md) for the full,
> source-verified audit and the 100-goal plan to close the gaps honestly
> (governing rule: an item is only "Resolved" once backed by a CI-enforced
> regression test). Three entries below are corrected inline with a note;
> everything else in this file is unchanged pending its own goal in that plan.
> The audit also surfaced a **critical, previously-undocumented finding**:
> `wasmtime`/`wasmtime-wasi` is pinned at `28.0.1`, which has over a dozen
> disclosed RUSTSEC advisories including several sandbox-escape and
> out-of-bounds-memory issues in the exact component (`crates/wasm-plugin-host`)
> that markets itself as running plugins "sandboxed." `cargo deny check
> advisories` reproduces this locally today. This needs a dedicated,
> carefully-verified wasmtime upgrade — out of scope for a quick fix, tracked
> as new work in `PRODUCTION-READINESS-100.md`'s Phase 1.
>
> **2026-09-15 update:** All 150 production-readiness goals across three
> audit batches have been implemented and compile cleanly under
> `cargo check --target x86_64-pc-windows-gnu`. The only remaining blocker
> is the `wasm-plugin-host` wasmtime v28 API migration (pre-existing).
>
> **2026-09-15 correction, Phase 7-9 pass:** the line above is no longer
> accurate on two counts. First, `wasm-plugin-host` isn't the *only*
> compile blocker — `pledgepack-core` itself intermittently fails to compile
> depending on the state of an in-progress, uncommitted `task_transform`
> module (see `PRODUCTION-READINESS-100.md`'s Phase 3-4 verification notes).
> Second, and much more seriously: **compiling cleanly says nothing about
> whether the resulting binary runs, and right now it doesn't** — the
> compiled `pledge` CLI segfaults on every invocation, including `pledge
> --version` with no other arguments, before any of this project's own code
> executes. Found 2026-09-15 while adding CLI tests (Phase 8 goal 89);
> current best hypothesis (not yet confirmed with a live debugger) points at
> a MinGW auto-import indirection (`.refptr.__stack_chk_guard`) in the
> compiled Zig static library not resolving correctly for a symbol that's
> statically linked into the same binary, rather than imported from a
> separate DLL — see `PRODUCTION-READINESS-100.md`'s "Known blockers"
> section for the full investigation. This is now the single most urgent
> open item across both documents.

---

## Platform Support

### Status: ✅ Resolved
CI (GitHub Actions) cross-compiles and publishes prebuilt binaries for 6 platform targets (Windows x64/arm64, Linux x64/arm64, macOS x64/arm64) on each release. The release workflow passes `-Dtarget` to `zig build` for correct cross-compilation of the Zig native library. `platforms.json` (repo root) is now the single source of truth for this list, consumed by both `release.yml`'s build matrix and `bin/postinstall.js`'s download logic, and checked for drift by `scripts/check-platform-lists.js` — this previously listed only 5 platforms and `bin/postinstall.js` had actually fallen out of sync with what `release.yml` published (missing `win32-arm64`).

---

## Parallel Transform Pipeline

### Status: ✅ Resolved
The transform pipeline now uses rayon for parallel module transformation. The build loop is split into a BFS resolution phase followed by parallel transformation, with cache population and graph wiring.

---

## Source Maps in Production

### Status: ✅ Resolved
`pledgepack build` emits source maps with `none`, `inline`, and `external` options. Oxc's native source map generation is used.

---

## CSS Bundling

### Status: ✅ Resolved
Lightning CSS is integrated for CSS minification, autoprefixing, dead code elimination, and CSS code splitting aligned with JS chunk boundaries.

---

## Code Splitting for Dynamic Imports

### Status: ✅ Resolved
The optimizer splits dynamic `import()` calls into separate lazy-loaded chunks. The import map includes chunk mappings for dynamic imports.

---

## JS Plugin System — Full API

### Status: � Partially resolved (corrected 2026-09-15 — see audit note above)
The JS plugin host (powered by QuickJS/rquickjs 0.12.2) supports module graph access (`get_module_info`), custom resolvers (`resolve_id`), HMR interception (`on_hmr_update`), and a `PluginContext` for passing graph data. The Vite-compatible API genuinely executes `resolveId`, `load`, `transform`, `transformIndexHtml`, and `configureServer` against the plugin's JS.

**Correcting two previously-inaccurate claims in this entry:**
- `buildStart`, `buildEnd`, and `generateBundle` are **detected but not executed** in `js-plugin-host` — `crates/js-plugin-host/src/lib.rs`'s `build_start()`/`build_end()`/`generate_bundle()` only log that the hook exists (`info!("[plugin:{}] buildStart", ...)`); the plugin's actual JS function is never called. The WASM host (`wasm-plugin-host`) *does* execute all three for real. `handleHotUpdate` (a common Vite hook) is absent from both hosts and from the WIT contract entirely. See `PRODUCTION-READINESS-100.md` Phase 3 (goals 41-50) for the plan to fix this — likely by consolidating onto a single host.
- Plugin signing verification (G12.35) is **not real cryptographic verification** today: `PluginSigningVerifier::verify()` in `crates/core/src/plugin_system.rs` checks only that the signature/hash/pubkey strings are non-empty; the real Ed25519 check is written as a comment in the same function. Neither it nor the capability audit (G12.36) is called from any actual plugin-loading path — both are exercised only by their own unit tests. See goals 11-13.
- `WasmPluginHostBridge` serializes all plugin calls through a single `Mutex` — a known bottleneck; a `PluginInstancePool` is stubbed but not yet implemented.

---

## Tree Shaking for CSS-in-JS

### Status: ✅ Resolved
Static analysis for styled-components and emotion removes unused style definitions during tree shaking.

---

## Hot Reloading for Server-Only Code

### Status: ✅ Resolved
Server-only file changes are detected via `compute_server_dirs()` and `is_server_file()`. The dev server sends `server-reload` → `server-reload-complete` HMR updates, preserving WebSocket connections. A client-side banner UI shows reload status.

---

## Import Map — Version Deduplication

### Status: ✅ Resolved
The auto-generated import map now includes `scopes` entries for packages with multiple versions in nested `node_modules`. Monorepo setups with conflicting dependency versions resolve correctly per-scope.

---

## Built-in Test Runner UI

### Status: ✅ Resolved
`pledgepack test --watch` provides an interactive terminal UI with coverage reporting and browser-based test runner support for component tests.

---

## HTTPS Dev Server

### Status: ✅ Resolved
`pledgepack dev --https` enables HTTPS with automatic self-signed certificate generation via `rcgen`. Custom certificates are supported via `https.cert` and `https.key` config.

---

## Incremental Build Watch Mode

### Status: ✅ Resolved
`pledgepack build --watch` uses the function-level incremental cache. On file change, only affected modules are re-transformed and changed chunks are re-emitted.

---

## Binary Size

### Status: ✅ Resolved
- Release profile uses `strip = true`, `lto = "fat"`, `opt-level = 3`, and `codegen-units = 1` for maximum optimization, with `panic = "unwind"` so panics can be caught and reported instead of killing the process outright.
- WASM plugin host crate re-added with wasmtime for first-class sandboxed plugins (WASM Component Model, WIT contract at `wit/world.wit`, currently frozen at v0.1.2). **Correction (2026-09-15):** this previously said "wasmtime v47"; `Cargo.lock` actually pins `wasmtime`/`wasmtime-wasi` at `28.0.1`, which — see the audit note at the top of this file — carries multiple disclosed sandbox-escape and memory-safety RUSTSEC advisories. Upgrading past 28.0.1 is tracked as urgent follow-up work, not yet scheduled as a specific goal number pending a compatibility review of the intervening wasmtime API changes.
- JS plugin host migrated from Boa to QuickJS (rquickjs 0.12.2) — ~500KB binary, 10-100x faster than Boa.
- Release binary includes Oxc, Lightning CSS, QuickJS JS runtime, wasmtime, notify, tokio, axum.

---

## PledgeStack Full-Stack Runtime

### Status: ✅ Resolved — Route Discovery + Manifest Generation

The PledgeStack adapter (`crates/adapter-pledgestack/`) provides comprehensive **route discovery and manifest generation**. Runtime execution (SSR, API route handling, middleware execution) is handled by PledgeStack itself (the framework layer), not PledgePack (the bundler layer). This is the correct architectural separation — PledgePack is a dumb bundler, PledgeStack is the full-stack framework.

- ✅ **Frontend route discovery** — `app/` directory scanning with dynamic routes, layouts, loading/error boundaries
- ✅ **API route discovery** — `app/api/*/route.ts` with HTTP method detection
- ✅ **Rust backend route discovery** — `server/api/*.rs` and `.psx` files with `#[route(...)]` macro parsing
- ✅ **Middleware discovery** — Root and server middleware files detected
- ✅ **`.psx` → `.rs` copy** — Files copied for `cargo build` compatibility
- ✅ **Route manifest generation** — JSON manifest with all frontend + backend + middleware routes
- ✅ **Project scaffolding** — `pledgepack create pledgestack` generates full app structure
- ✅ **Architecture separation** — PledgePack handles bundling/serving; PledgeStack handles SSR/API/middleware runtime (see [CONNECTION.md](./CONNECTION.md))

> **Note:** PledgePack deliberately does NOT implement SSR rendering, API route execution, middleware execution, or `.psx` transpilation — these are PledgeStack's responsibilities. See [CONNECTION.md](./CONNECTION.md) for the full responsibility split.
