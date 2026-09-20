# PledgePack — Roadmap

> See [CHANGELOG.md](./CHANGELOG.md) for full release history.

---

## Status

| Goal Set | Total | Complete | Status |
|----------|-------|----------|--------|
| Foundation Roadmap (Phases 0–5 + 7 polish) | 13 | 13 | ✅ Core done |
| Rival Goals (beat every competitor) | 194 | 194 | ✅ Core done |
| PledgePack Goals (v3: 85 goals) | 85 | 85 | ✅ Core done |
| PledgeJS Integration Goals (1–106) | 106 | 106 | ✅ Core done |
| Production Readiness — Batch 1 (security/correctness) | 50 | 50 | ✅ Implemented |
| Production Readiness — Batch 2 (transforms/HMR/resolver/server) | 50 | 50 | ✅ Implemented |
| Production Readiness — Batch 3 (remaining gaps) | 50 | 50 | ✅ Implemented |
| **wasmtime 28 → 48 bump** | — | — | ✅ Committed 2026-09-17 (`75946ba`) — advisories cleared |
| **wasm-plugin-host compile break** | — | — | ✅ Resolved 2026-09-17 — crate compiles + all tests pass (40 unit + 20 e2e) on wasmtime 48; exclusion removed from CI |

> **Note:** Feature completeness and production hardening are separate tracks.
> All 150 production-readiness goals across three audit batches are implemented
> and compile cleanly under `cargo check --target x86_64-pc-windows-gnu`. The
> only remaining blocker is the `wasm-plugin-host` crate, which doesn't
> compile as a whole crate and is excluded from workspace checks (see
> LIMITATIONS.md). Its dep was bumped to wasmtime 48.0.2 on 2026-09-17 but
> the crate's WASI code still targets the 28-era API and has never been
> checked on 48. **[Fully resolved later 2026-09-17 — the crate compiles,
> all its tests pass on wasmtime 48, and it's back in CI.]**
>
> **2026-09-15 correction — this "Status" table and the "✅ Implemented" batch
> summaries below predate a source-level audit and are self-reported, not
> test-verified.** The individual ✅ items were not re-checked line-by-line in
> this pass (300+ entries — out of scope to re-verify all of them at once);
> treat them as a historical changelog of intent, not a current-state
> guarantee. Two things in the "compile cleanly" claim above are now known to
> be **false**: (1) `pledgepack-wasm-plugin-host` cannot be compiled as a
> whole crate — not only because of the wasmtime migration noted above, but
> because `WasmPluginHostBridge` depends on a `task_transform` module that
> exists on disk but isn't wired into `pledgepack-core`'s module tree (an
> in-progress, uncommitted feature); (2) far more seriously, **the compiled
> `pledge` CLI binary itself segfaults on every invocation** — including
> `pledge --version` with no other arguments — on at least one Windows dev
> machine, found 2026-09-15. "Compiles cleanly" was never a claim that the
> resulting binary runs, and right now it doesn't. See
> [`PRODUCTION-READINESS-100.md`](PRODUCTION-READINESS-100.md) — governing
> rule: a goal counts as done only once backed by a passing, CI-enforced
> regression test, not by "it compiled" — for the source-verified status of
> Phases 0-8 and the full writeup of both findings above (search "Known
> blockers").

---

## Production Readiness Progress

Three audit batches have been implemented. Below is what was completed.

### Batch 1 — Security & Correctness (50 goals)

#### Security
- ✅ Dev-server path traversal fixed in `module_handler`/`public_dir_handler` (canonicalization)
- ✅ `/@fs/` `contains("node_modules")` bypass removed — canonical path checks
- ✅ SHA256 checksum verification in `bin/postinstall.js`
- ✅ Cosign artifact signing in release workflow
- ✅ Remote cache subprocess calls replaced with native HTTP client
- ✅ SRI generation path traversal validation
- ✅ `prepare_psx_files()` path traversal validation
- ✅ Regex HTML parsing replaced with `scraper` for SRI/CSP
- ✅ HMR WebSocket `ws://`/`wss://` based on `location.protocol`
- ✅ Proxy body size limit added
- ✅ WASM `restricted_wasi_ctx()` stdio discarded per docs
- ✅ Git dependencies pinned (`unknown-git = "deny"`)

#### Correctness — Adapters
- ✅ React/Solid adapter non-panicking parser errors now bail
- ✅ Next.js adapter clears routes/middleware before rescan
- ✅ Next.js parallel route `slot`/`intercept` fields populated
- ✅ Next.js generated router uses App Router integration
- ✅ TanStack generated router uses `createRouter`/`RouterProvider`
- ✅ PledgeStack `detect_route_methods`/`detect_render_mode` uses line-aware matching

#### Correctness — Task System & Graph
- ✅ 256-node limit removed from Zig `getInvalidationSet()` and Rust `invalidation_set()`
- ✅ `Environment::Custom` collision fixed
- ✅ `register_custom()` collision detection added
- ✅ `batch_schedule()` returns `TaskError::CycleDetected`
- ✅ `AggregationGraph` cycle detection
- ✅ `Notify` dedup replaced with multi-waker broadcast
- ✅ `ReadTracker` path canonicalization

#### Correctness — Core Engine
- ✅ `run_dev_build` implemented
- ✅ Optimizer state reset between calls
- ✅ `inline_dynamic_imports` merges per-entry only
- ✅ tsconfig comment stripper replaced with JSON5 parser
- ✅ `generate_config_schema()` propagates errors
- ✅ `pre_transform_closure()` filters to `enforce: "pre"` plugins only

#### Memory Safety / Unsafe Code
- ✅ `AstPool` lifetime erasure stabilized
- ✅ `MAP_SHARED` mmap `msync` check added
- ✅ `unsafe impl Sync` on `Graph` removed (replaced with `Mutex`)
- ✅ `# Safety` docs added to `AstPool` methods
- ✅ `ftell()` checked for `-1` in Zig `readFile()`

#### Error Handling
- ✅ Cache directory creation failure no longer silently ignored
- ✅ Remote cache store failures return `Err`
- ✅ Bincode key serialization fallback produces distinct hashes
- ✅ Remote cache `set` errors logged
- ✅ `WasmPluginHost::default()` returns `Result` instead of panicking

#### Performance
- ✅ Optimizer `Vec::contains` → `HashSet`
- ✅ Zig io_uring/IOCP/kqueue batch reads documented as stubs
- ✅ NUMA/huge-page stubs documented
- ✅ `WasmPluginHostBridge` single-Mutex documented

#### Testing & CI
- ✅ Path-traversal security tests in dev-server e2e
- ✅ WASM E2E tests fail (panic) when guest artifact missing
- ✅ `cargo-llvm-cov` coverage job in CI
- ✅ Windows ARM64 release target
- ✅ Dependabot config

### Batch 2 — Transforms / HMR / Resolver / Dev Server / Native FFI (50 goals)

#### Transforms — JS
- ✅ Fast Refresh type mismatch — `__pledge_fast_refresh` initialized as callable function
- ✅ Bail on non-panicking JS parse errors
- ✅ Minifier `compress` options enabled
- ✅ `is_react_component` regex-based detection (OnceLock cached)
- ✅ `extract_component_name` fixed `const`/`export const` parsing
- ✅ Browser targets passed to Oxc transformer
- ✅ Decorator support via `experimental_decorators` + tsconfig detection
- ✅ Transform diagnostics propagate as errors (`has_errors()`)

#### Transforms — CSS
- ✅ CSS module class extraction — proper selector parsing (skips `url()`, strings, comments)
- ✅ Production CSS source maps generated when `config.source_maps` is true
- ✅ CSS minify skipped in dev mode
- ✅ CSS nesting transpilation (`flatten_nesting`)
- ✅ Browser targets wired to Lightning CSS
- ✅ Dark mode `auto` handles component-scoped custom properties
- ✅ CSS-in-JS extraction skips strings/comments
- ✅ Escaped/unicode CSS class names handled

#### Transforms — SFC
- ✅ Vue `v-if`/`v-else`/`v-for` as real conditionals (ternary render logic)
- ✅ Vue `v-model` for components, checkboxes, selects, modifiers
- ✅ Svelte `{#if}`/`{:else}`/`{/if}` and `{#each}` blocks
- ✅ Svelte `$:`/`$store`/lifecycle documented
- ✅ Source maps for Vue, Svelte, Astro transforms
- ✅ `extract_sfc_blocks` with depth tracking, attributes, multiple blocks

#### Transforms — Optimizations & Data
- ✅ Constant folding skips strings/comments/regex
- ✅ Boolean folding skips strings/comments/regex
- ✅ Cross-chunk hoisting uses exact binding match
- ✅ Side-effect analysis handles template literals
- ✅ Source maps for MDX, GraphQL, YAML, CSV, TSV, TOML
- ✅ JSON keys with hyphens/dots preserved

#### HMR
- ✅ `accept()` callbacks invoked on update
- ✅ CSS Modules HMR class-name remapping
- ✅ `sourceMappingURL` appended to dev JS responses
- ✅ `HmrDiffConfig` configurable thresholds

#### Resolver
- ✅ pnpm symlinked `node_modules` layout support
- ✅ `browser` field object mapping support
- ✅ Alias boundary enforcement (longest-first + path boundary)
- ✅ `imports` field (`#` subpaths) resolution
- ✅ Context-driven condition priority (`ResolveRuntime`/`ResolveModuleType`)

#### Module Graph & Engine
- ✅ Cycle detection during graph construction (`can_reach`)
- ✅ Deterministic topological ordering (`sort_unstable`)
- ✅ `emit_with_chunks` wires optimizer chunk boundaries
- ✅ `import.meta.glob` lazy import generation

#### Dev Server / Production
- ✅ Graceful SIGINT/SIGTERM shutdown (`with_graceful_shutdown`)
- ✅ `RequestBodyLimitLayer` (10MB)
- ✅ Security headers (`X-Frame-Options`, `X-Content-Type-Options`, `Referrer-Policy`)
- ✅ Brotli compression (`.br(true)`)
- ✅ ETag/Last-Modified conditional requests
- ✅ SPA fallback for preview/serve
- ✅ CORS middleware (`CorsLayer`)

#### Native FFI
- ✅ `read_file` memory leak fixed (`pledge_io_free`)
- ✅ `find_imports` buffer increased 1024 → 65536
- ✅ Null-pointer validation on all FFI returns
- ✅ `stack_chk_guard` randomized at startup (`init_stack_canary`)
- ✅ Cache entry versioning (`CACHE_FORMAT_VERSION`)

#### Error Handling & Config
- ✅ `.env` precedence fixed (process env wins)
- ✅ Webhook errors propagated (`WebhookError`), HMAC signing, timeout
- ✅ `--config` TypeScript parsing (reuses normal config loader)
- ✅ `pledge clean` command
- ✅ `pledge update` self-update command
- ✅ `validate_config_values()` — types, ranges, cross-field constraints
- ✅ XOR encryption deprecated, empty key panic fixed

#### Bonus
- ✅ Tracing spans for build phases (resolve/parse/transform/emit)
- ✅ PostCSS `@import url()` + media query support
- ✅ PostCSS source map generation
- ✅ Template dependency versions extracted to constants
- ✅ `import.meta.env` TypeScript declarations (`generate_env_dts`)
- ✅ HMR serialization `unwrap_or_default` fixed
- ✅ WebSocket per-message-deflate documented (not yet enabled)
- ✅ Invalid glob patterns logged as warnings
- ✅ `import.meta.glob` source map support

### Batch 3 — Remaining Gaps (50 goals)

#### JS Transform
- ✅ `extract_component_name` regex parsing fixed
- ✅ Browser target configuration wired
- ✅ Decorator support configured
- ✅ Transform diagnostics propagate as errors
- ✅ `is_react_component` regex cached in `OnceLock`

#### CSS Transform
- ✅ CSS nesting `flatten_nesting()` implemented
- ✅ Browser targets passed to Lightning CSS
- ✅ Dark mode handles component-scoped properties
- ✅ CSS-in-JS string/comment skipping
- ✅ Escaped/unicode class names handled

#### Environment Variables
- ✅ Multiline `.env` values (quoted, backslash continuation)
- ✅ `${VAR}` cycle detection (depth limit)
- ✅ Dead branch elimination improved (paren-depth, both operand orders)

#### Asset Pipeline & HTML
- ✅ CSV parser handles quoted fields with commas
- ✅ GraphQL keyword boundary checking (`is_at_definition_boundary`)
- ✅ SVG handling integrated
- ✅ HTML parsing uses `scraper` (not string search)
- ✅ HTML comment detection bug fixed
- ✅ `extract_title` handles `<title>` with attributes

#### Dev Server
- ✅ `module_cache` bounded (LRU eviction, `MAX_MODULE_CACHE_SIZE`)
- ✅ `import_graph` bounded (`MAX_IMPORT_GRAPH_SIZE`)
- ✅ HMR diff `Insert` line number fixed
- ✅ `is_path_within` canonicalization (symlink-safe)
- ✅ `UnixListener` support + `--socket` CLI flag
- ✅ Response size limit (`MAX_RESPONSE_SIZE`)

#### Optimizer & Module Graph
- ✅ `tree_shake` depth limit removed (`usize::MAX`)
- ✅ Side-effect detection handles IIFEs, top-level `await`
- ✅ `reverse_deps` deduplication
- ✅ `can_reach` follows dynamic dependencies
- ✅ `MAX_MODULES` limit + `.pledge.lock` file locking

#### Engine & Cache
- ✅ `parallel_fetch` uses `std::thread::scope`
- ✅ `DedupCache` persistence (`persist()`/`load()`)
- ✅ `derive_signing_keypair` deterministic BLAKE3 KDF
- ✅ `function_cache` wrapped in `DashMap`
- ✅ `panic = "unwind"` in release profile

#### CLI & Metadata
- ✅ `description`/`license`/`repository`/`keywords`/`categories` on all crates
- ✅ `lightningcss` alpha documented
- ✅ `noyalib` documented with TODO
- ✅ CLI error handling (`eprintln!` for errors)

#### Cross-Platform
- ✅ `normalize_path()` utility for consistent `\` → `/` conversion
- ✅ CRLF normalization for source files
- ✅ Path canonicalization for symlink safety

#### Tests
- ✅ Optimizer tests (`#[cfg(test)]` module)
- ✅ Resolver tests (`#[cfg(test)]` module)
- ✅ Cache tests (`#[cfg(test)]` modules in `lib.rs`, `advanced.rs`, `remote.rs`, `git_cache.rs`)
- ✅ HMR diff tests (`#[cfg(test)]` module)
- ✅ JS/CSS/env transform edge case tests

---

## Remaining Work

### Blocked
- ~~**wasm-plugin-host doesn't compile**~~ — **resolved 2026-09-17**: `task_transform` + `ast_pool` were wired into `pledgepack-core`'s module tree (they existed on disk but were never declared), the WASI integration was migrated to the wasmtime-48 API (`WasiCtxView`, `p2::add_to_linker_sync`, `HasSelf` bindgen accessor), and the e2e fixture was rebuilt for `wasm32-wasip2`. The crate now compiles clean under `clippy -D warnings` and passes 40 unit + 20 e2e tests on wasmtime 48.0.2; the workspace `--exclude pledgepack-wasm-plugin-host` flags were removed from CI.

### Future
- Real-world testing at scale and edge case discovery
- Performance benchmarking and memory profiling
- Plugin marketplace and community ecosystem
- PledgeStack adapter end-to-end validation with real applications
- PostCSS Node.js subprocess for full plugin ecosystem support

---

## What Was Built

### Foundation (Phases 0–5)
- Phase 0: WIT plugin contract frozen at v0.1.2, WASM validation complete
- Phase 1: Task graph substrate — `Task<T>`, `DependencyGraph`, `TaskEngine`, Zig `TaskGraph`
- Phase 2: WASM plugin host — wasmtime 48.0.2, sandboxed, 9 hooks, AOT compilation (compiles + 40 unit / 20 e2e tests green as of 2026-09-17 — see LIMITATIONS.md)
- Phase 3: JS plugin shim — QuickJS (rquickjs 0.12.2), content-addressed caching
- Phase 4: Shared AST — `AstPool` parse-once, dynamic import detection, i18n extraction
- Phase 5: Async scheduler — `transform_via_task_engine()` with `tokio::task::JoinSet`
- Polish: Plugin ordering, host imports, `renderChunk` hook, cache analytics, HMR debounce

### Rival Goals (194)
All 194 goals across 12 dimensions complete: Task type, `#[task]` macro, aggregation graph, caching, plugin ABI, dev server, observability, determinism, DX, ecosystem, speed, memory.

### PledgePack v3 Goals (85)
All 85 goals across Developer Experience, Differentiation, Plugin Ecosystem, and Developer Tooling complete.

### PledgeJS Integration (106)
All 106 integration verification goals complete: PSX transform pipeline, dev server & HMR, build output, framework adapters, binary distribution, E2E testing, performance benchmarks, error handling, cross-platform CI.

---

## Next Focus

1. **wasm-plugin-host** ~~compile break~~ — **resolved 2026-09-17** (see Blocked above); next step for the WASM tier is real-world plugin coverage beyond the test fixture
2. **Real-world validation** — Production testing at scale, edge case discovery, memory profiling
3. **PledgeStack integration** — End-to-end framework validation with real applications
4. **PostCSS ecosystem** — Node.js subprocess for full PostCSS plugin support
5. **Ecosystem adoption** — Plugin marketplace, community presets, documentation site
