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
> **[Resolved 2026-09-17 — wasmtime 48.0.2 landed in `75946ba`; advisories
> check is green.]**
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
> whether the resulting binary runs** — the compiled `pledge` CLI was
> segfaulting on every invocation at the time this note was written.
>
> **2026-09-17 update:** the segfault was root-caused and fixed the same day
> (commit `3e5874b` — `native-sys` defined MSVC-only runtime shims
> unconditionally; on GNU toolchains they duplicated MinGW's own symbols and
> corrupted stack probing on rayon worker threads — regression test:
> `native-sys/tests/rayon_compat.rs`). The remaining known blockers are now:
> (1) ~~`wasm-plugin-host` can't compile as a whole crate~~ — **resolved
> 2026-09-17**: `task_transform` (plus the also-unwired `ast_pool`) was wired
> into `pledgepack-core`'s module tree, the WASI integration was migrated to
> the wasmtime-48 API (`WasiCtxView`, `p2::add_to_linker_sync`, `HasSelf`
> bindgen accessor), and the crate now compiles and passes all tests — 40
> unit + 20 e2e on wasmtime 48.0.2. The crate is no longer excluded from
> CI's workspace build/clippy/test commands, and the e2e fixture was
> rebuilt for `wasm32-wasip2` (the wasip1 artifact was a core module, not a
> component — the e2e suite had never actually run in this environment);
> (2) ~~`wasmtime`/`wasmtime-wasi` 28.0.1's disclosed RUSTSEC advisories~~ —
> **resolved later the same day** (commit `75946ba`): bumped to 48.0.2,
> `cargo audit`/`cargo deny` now pass; (3) ~~the `#[ignore]`d heap-corruption
> resolver test on Windows (goal 54)~~ — **resolved 2026-09-17**: the crash
> was a symptom of the MSVC/GNU runtime-shim bug fixed in `3e5874b`; the
> `#[ignore]` was removed and all 9 resolver tests pass.

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

### Status: 🟡 Mostly resolved (updated 2026-09-20 for 1.0.0-rc.1)

The JS plugin host (QuickJS / rquickjs 0.12) executes the plugin's own JS for
every hook it declares: `resolveId`, `load`, `transform`, `renderChunk`,
`transformIndexHtml`, `configureServer`, and — as of this release —
`buildStart`, `buildEnd`, `generateBundle` and `handleHotUpdate`. Each is covered
by a test that asserts on the plugin's observable side effects
(`crates/js-plugin-host/src/lib.rs`, `mod hooks`).

**What is real now**
- **Lifecycle hooks** (`buildStart`/`buildEnd`/`generateBundle`) run the plugin's
  function in order across plugins. Promise-returning (`async`) hooks are awaited
  by draining the QuickJS microtask queue. A hook that throws or rejects does not
  stop later plugins from running, but the failures are returned as an `Err` and
  `pledgepack build` aborts (matching Rollup/Vite). `pledgepack build` runs `buildStart`
  before the build and `generateBundle` + `buildEnd` after the output is emitted.
  `generateBundle` receives `({}, {})` — the emitted files are already on disk;
  the Rollup `bundle` object is not populated yet.
- **`handleHotUpdate(file, timestamp)`** runs in the dev server's file-watcher
  path with Vite semantics: `null` → default HMR update; `{ moduleIds: [] }` →
  suppress the update; `{ moduleIds: [...] }` → update exactly those modules.
  Plugins are loaded from `<root>/plugins/` (same trust policy as below).
- **Plugin signature verification is real Ed25519.** `PluginSigningVerifier::verify()`
  (`crates/core/src/plugin_system.rs`) requires the signer's `(identity, public key)`
  to be in the trusted set, then cryptographically verifies the signature over the
  plugin's blake3 content hash. Both plugin hosts read a `<plugin>.sig.json` sidecar,
  recompute the hash of the file on disk (so tampering is detected), verify, and
  then audit any declared capabilities before loading. It is wired into
  `pledgepack build` (`config.plugins`) and the dev server (`plugins/`). Tests cover
  valid, invalid-signature, tampered-source, untrusted-signer, missing-sidecar and
  malformed-sidecar cases for the JS host end to end.
- **Design decision — secure by default.** `plugin_security.requireSigned` defaults
  to **true**: with no `trustedKeys` configured every plugin is refused. Opt out
  explicitly with `plugin_security.requireSigned = false` (documented as insecure,
  intended for local plugin development). `pledgepack plugin keygen` / `pledgepack plugin
  sign <file>` produce keys and sidecars.
- **Fixed:** hook dispatch used to locate a plugin's JS global by *name*, so two
  plugins with the same `name` shared one module; it now uses the plugin's index.

**Remaining gaps (honest list)**
- **Per-module hooks are wired into `pledgepack build` (2026-09-20)** through the
  `pledgepack_core::plugin_hooks::PluginHooks` trait (core cannot depend on the host
  crate; the CLI passes its `JsPluginHost` as `&dyn PluginHooks` to
  `BuildEngine::build_with_hooks` / `emit_with_chunks_hooks`). Semantics are Rollup/Vite's:
  `resolveId` and `load` — first non-null result wins; `transform`, `renderChunk` and
  `transformIndexHtml` — chained in plugin order; `renderChunk` and `transformIndexHtml`
  run **before** content hashing, so chunk hashes reflect the final code. A plugin
  that throws or rejects aborts the build with the plugin name, hook and file. Covered by
  unit tests with a fake host (`plugin_hooks.rs`, `engine.rs`) and an end-to-end
  `pledgepack build` test with a real JS plugin (`crates/cli/tests/build_plugins_e2e.rs`).
  Limits of the wiring:
  - `transform` hooks run over the *loaded source before* PledgePack's built-in
    TS/JSX/CSS transform (so dependency discovery, the content hash and every cache see
    the post-plugin source). `enforce: "post"` placement after the built-in transform is
    **not** supported in the production pipeline; plugins receive TypeScript/JSX source,
    not compiled JS. Binary assets (`Asset`, `Wasm`) are never passed to `transform`.
  - Hooks run sequentially on the build thread (QuickJS is not `Send`), so plugin
    `transform` work is not parallelised and its results are not stored in the task
    cache — they re-run every build. Source maps returned by `transform` are ignored;
    `renderChunk` maps are used only when the plugin supplies one.
  - The dev server still does not run per-module hooks (see next item).
  - Fixed on the way: `pledgepack build` resolved `plugins` paths against the working
    directory (not `--root`) and silently skipped missing / failing plugins; it now
    resolves them against the project root and refuses to build if a configured plugin
    is missing or fails to load.
- In the dev server, with HMR on, the `plugins/` directory is loaded once, on the
  file-watcher thread (QuickJS contexts are not `Send`); only `configureServer` and
  `handleHotUpdate` see it (no per-module hooks, as above).
- QuickJS has no timers or I/O: an `async` hook that waits on anything other than
  already-resolved promises will be reported as "did not settle" (warning) rather
  than awaited.
- `WasmPluginHostBridge` still serializes all plugin calls through one `Mutex`.
  `PluginInstancePool::acquire_or_load` now lets callers lazily create pooled
  instances, but the bridge does not use the pool yet.
- The WASM host's `resolve-import` host call is now delegated to an
  embedder-supplied resolver (`WasmPluginHost::with_import_resolver`); the CLI does
  not install one yet, so plugins still see "unresolved" by default.
- `generate_wasm_skeleton()` (the would-be `plugin migrate`, not yet wired to
  a CLI subcommand) emits a signature scaffold, not a ported plugin.

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

## Dependencies (accepted risks, 2026-09-20)

### Status: 🟡 One accepted risk, re-review by 2026-12-15
- **lightningcss 1.0.0-alpha.72** — there is no stable release: alpha.72 is the newest
  version on crates.io, so there is nothing to upgrade to. Accepted, with mitigation:
  it is built with `default-features = false` (we only use stylesheet
  parse/minify/print + targets), which removes `parcel_sourcemap` and its vulnerable
  `rkyv 0.7` (RUSTSEC-2026-0235) from the tree — that advisory no longer needs an
  ignore. Tracked with a `review-by` marker in the workspace `Cargo.toml`
  (enforced by `scripts/check-stale-ignores.sh`).
- **noyalib** (0.0.x, pre-1.0) was replaced by **serde-saphyr 1.x** for YAML imports.
- Remaining audit ignores (all "unmaintained", no CVE; each with a review-by date):
  `bincode` 2.0.1, `paste`, `fxhash` — see `.cargo/audit.toml` and `deny.toml`.
  `cargo audit` reads `.cargo/audit.toml`; do not repeat `--ignore` flags in CI.

---

## Svelte SFC Support

### Status: 🟡 Partial (documented, not a bug)
The Svelte compiler path lowers templates, `{#if}`/`{:else}` and one level of `{#each}`.
Not compiled (passed through / relies on a runtime shim): `$:` reactive statements,
`$store` auto-subscriptions, `{#await}` blocks, lifecycle hooks, `{:else if}` chains
(the first else-family marker is the boundary) and control flow nested inside
`{#if}`/`{#each}`. Use the official Svelte compiler for projects that need them.

---

## Visual Regression (`pledgepack test --visual`)

### Status: ✅ Implemented (updated 2026-09-20) — needs a browser
Previously this fabricated a 1×1 placeholder PNG and compared raw bytes, so it could
never fail meaningfully. It now captures real screenshots with a headless
Chrome/Chromium/Edge (`PLEDGE_CHROME` to point at one) and compares decoded pixels;
it **errors** (never silently passes) when no browser is found. The page at
`dev_server.port` must already be serving — the command does not start a server.

---

## Dev Server WebSocket Compression

RFC 7692 per-message-deflate is not enabled for the HMR socket (axum's upgrade API
does not negotiate it). HTTP responses are compressed; HMR payloads are small.

---

## Binary Size

### Status: ✅ Resolved
- Release profile uses `strip = true`, `lto = "fat"`, `opt-level = 3`, and `codegen-units = 1` for maximum optimization, with `panic = "unwind"` so panics can be caught and reported instead of killing the process outright.
- WASM plugin host crate re-added with wasmtime for first-class sandboxed plugins (WASM Component Model, WIT contract at `wit/world.wit`, currently frozen at v0.1.2). **Correction (2026-09-15):** this previously said "wasmtime v47"; `Cargo.lock` actually pins `wasmtime`/`wasmtime-wasi` at `28.0.1`, which — see the audit note at the top of this file — carries multiple disclosed sandbox-escape and memory-safety RUSTSEC advisories. **2026-09-17 update:** the bump to wasmtime `48.0.2` has landed (commit `75946ba`), clearing all 20 advisories — `cargo audit` and `cargo deny check` now exit 0. `wasm-plugin-host`'s `task_transform` compile break was fixed the same day: the crate compiles under `clippy -D warnings`, passes 40 unit + 20 e2e tests on wasmtime 48.0.2, and is no longer excluded from CI's workspace commands.
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

---

## Build engine notes (2026-09-20)

Fixed in this pass (each with a regression test in `crates/core/src/engine.rs`):

- The in-memory function cache was keyed by content hash alone, so two files with
  identical text at different paths (or `x.ts` vs `x.tsx`) shared one entry — and one
  output. The key now mixes in the path and module kind (`engine::module_cache_key`).
- The legacy incremental path (`PLEDGE_LEGACY_ENGINE`) compared module *ids* across
  builds, but ids are assigned in discovery order and shift; it now matches modules by
  path. (The default task-graph pipeline never used this path.)
- `emit_with_chunks` wrote only the first module's source map for a multi-module chunk.
  It now writes a merged *indexed* source map (`sections`, one per module at its line
  offset) and appends a single `sourceMappingURL` for the chunk. If a `renderChunk` plugin
  changes the code, the merged map no longer applies and none is written unless the plugin
  returns one.
- A chunk that references a module missing from the transform cache (or the build) used
  to silently drop it from the bundle; it is now a build error.
- Chunk ids (optimizer / `manual_chunks` names) are sanitised to `[A-Za-z0-9._-]` before
  being used in file names; the manifest keeps the original id.
- `*.worker.js` / `*.worker.ts` files are now classified as `ModuleKind::Worker`
  (`ModuleKind::from_path`); the old extension-only lookup could never match a compound
  extension. `*.wc.tsx` / `*.wc.jsx` still classify as plain TSX/JSX (the `WebComponent`
  arm of `from_extension` has the same dead-code problem, but switching it on would
  change how those files compile, so it was left alone).
- The import scanner mistook `export const s = 'a from b'` for a re-export because the
  word `from` appeared inside a string value, creating a phantom dependency.

Still open:

- ~~The engine has its own module resolver (`BuildEngine::resolve`: aliases, relative
  paths, `node_modules`, package `exports`) that duplicates `crates/resolver`.
  `pledgepack-resolver` depends on `pledgepack-core`, so core cannot depend on it
  without a cycle; unifying them means moving the resolver below core (a larger
  refactor). Consequences: the engine does not support `package.json` `imports`
  (`#subpath`) or the workspace-aware resolution that `pledgepack-resolver`
  implements.~~ — **resolved**: the dependency direction was inverted
  (`pledgepack-core` now depends on `pledgepack-resolver`) and
  `BuildEngine::resolve` delegates to the shared `Resolver` built from the same
  `PledgeConfig` via `module_resolver`, adding only the `/__pledge_router`
  virtual module and a root-relative fallback for non-module specifiers. The
  engine now honours `imports`/`#subpath` and workspace packages. A conformance
  suite (`crates/core/tests/resolver_conformance.rs`) runs identical fixtures
  through both surfaces so the delegation cannot silently diverge. Resolved
  paths are canonicalized (verbatim `\\?\` on Windows), which can surface in
  emitted code and diagnostics.
- `adapter-react` / `adapter-solid` now return a source map from their standalone
  `transform` (`ReactTransformResult::source_map`, `SolidAdapter::transform_with_source_map`),
  but the production build does not call the adapters — JSX/TSX is compiled by
  `pledgepack_core::transform`, which already emits maps — so this only benefits
  embedders using the adapters directly.
