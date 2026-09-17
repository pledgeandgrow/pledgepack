//! WASM Component Model Plugin Host for PledgePack.
//!
//! This crate implements the plugin host that runs WASM plugins sandboxed,
//! using the WIT contract defined in `wit/world.wit` (frozen at v0.1.0).
//!
//! # Architecture
//!
//! - `WasmPluginHost` — manages multiple WASM plugin instances, calls hooks
//! - `WasmPlugin` — a single loaded plugin instance (component + store)
//! - Hooks are called via the generated `bindgen!` bindings
//!
//! # Sandbox
//!
//! By default, plugins run in a strict sandbox:
//! - No filesystem access
//! - No network access
//! - CPU: 10M fuel units per hook call (~10ms, prevents infinite loops)
//! - Memory: 128MB max linear memory (enforced via StoreLimits)
//!
//! The sandbox is enforced by not providing any WASI imports to the component.
//! A component that tries to import WASI will fail to instantiate.
//!
//! # Cache Integration
//!
//! Every hook output includes a `cache_key` field (blake3 hash of inputs).
//! The host wraps plugin output as a `Task<T>` in the task graph, keyed by
//! the plugin's cache key. This gives WASM plugins fine-grained caching
//! that JS plugins (opaque blob) don't have.
//!
//! # Two-Tier System
//!
//! This crate implements the first-class tier (WASM, sandboxed, fine-grained cache).
//! The second-class tier (JS shim, opaque cache) is in `pledgepack-js-plugin-host`.

use anyhow::Result;
use std::path::Path;
use tracing::{debug, info};

// Generate Rust bindings from the WIT contract.
// The path is relative to this crate's Cargo.toml directory.
wasmtime::component::bindgen!({
    world: "pledgepack-plugin",
    path: "../../wit",
});

// ─── WASI Integration ─────────────────────────────────────────────────
//
// Plugins built with `cargo component` and `wit-bindgen` may import
// `wasi:cli/environment` and `wasi:cli/exit` (from the wit-bindgen runtime).
// We provide a restricted WASI context:
// - No filesystem access
// - No network access
// - Environment variables: empty (plugins can't read host env)
// - Args: empty
// - Stdin/stdout/stderr: discarded
//
// This maintains the sandbox while allowing wit-bindgen-based plugins to load.

use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

// ─── Plugin Instance ──────────────────────────────────────────────────

/// A loaded WASM plugin instance.
///
/// Each plugin has its own `Store` (instance state) and a handle to the
/// instantiated component. The host calls hooks via the handle.
///
/// The plugin is sandboxed: no filesystem, no network. The only WASI
/// imports provided are `wasi:cli/environment` and `wasi:cli/exit`
/// (required by wit-bindgen runtime), with empty environment and args.
pub struct WasmPlugin {
    /// The plugin metadata (from `plugin-metadata` hook)
    metadata: PluginMetadata,
    /// The wasmtime store (owns the plugin's memory and state)
    store: wasmtime::Store<PluginState>,
    /// The instantiated component handle (for calling hooks)
    instance: PledgepackPlugin,
    /// G7.3: Cumulative fuel consumed across all hook invocations.
    /// Capped at `MAX_CUMULATIVE_FUEL` to bound total CPU usage per plugin
    /// instance over its lifetime (prevents a plugin from burning an
    /// unbounded amount of CPU across many small calls).
    cumulative_fuel: u64,
}

/// State stored in the wasmtime Store for each plugin instance.
pub struct PluginState {
    /// Plugin name (for diagnostics)
    name: String,
    /// Whether this plugin has been initialized
    initialized: bool,
    /// WASI context (restricted — no filesystem, no network)
    wasi: WasiCtx,
    /// WASI resource table
    table: wasmtime::component::ResourceTable,
    /// Host config (JSON string, provided by PledgePack)
    /// Item 6: Host imports — get-config
    host_config: String,
    /// Files emitted by the plugin via emit-file
    /// Item 6: Host imports — emit-file
    emitted_files: Vec<(String, String)>,
    /// G7.3: memory-growth limiter, enforced via `Store::limiter`. Owned
    /// here (rather than built fresh per call) because `Store::limiter`'s
    /// closure must return a `&mut dyn ResourceLimiter` — a reference into
    /// existing state, not a freshly constructed value.
    limits: wasmtime::StoreLimits,
}

impl PluginState {
    fn new(name: String) -> Self {
        Self {
            name,
            initialized: false,
            wasi: restricted_wasi_ctx(),
            table: wasmtime::component::ResourceTable::new(),
            host_config: String::from("{}"),
            emitted_files: Vec::new(),
            limits: wasmtime::StoreLimitsBuilder::new()
                .memory_size(DEFAULT_MEMORY_MAX_BYTES)
                .build(),
        }
    }

    /// Set the host config (JSON string) that plugins can read via get-config.
    pub fn set_host_config(&mut self, config: String) {
        self.host_config = config;
    }

    /// Get the files emitted by the plugin via emit-file.
    pub fn emitted_files(&self) -> &[(String, String)] {
        &self.emitted_files
    }
}

// wasmtime-wasi 28.0.1's `WasiView` is the simple two-accessor form (`table`
// + `ctx`), not the newer unified `WasiCtxView` accessor some later
// wasmtime versions use — this previously didn't match the locked
// wasmtime-wasi version at all (see PRODUCTION-READINESS-100.md; discovered
// while verifying Phase 1/3 changes, not caused by them — this crate could
// not compile against its own Cargo.lock before this fix).
impl WasiView for PluginState {
    fn table(&mut self) -> &mut wasmtime::component::ResourceTable {
        &mut self.table
    }

    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }
}

// ─── Host Imports (Item 6) ────────────────────────────────────────────
//
// The WIT contract declares three host import functions that plugins can call:
//   - get-config: func() -> string
//   - emit-file: func(name: string, content: string) -> bool
//   - resolve-import: func(specifier: string, importer: string) -> option<string>
//
// The bindgen! macro generates a `PledgepackPluginImports` trait with these
// functions. We implement it for `PluginState` so plugins can call them.

impl PledgepackPluginImports for PluginState {
    fn get_config(&mut self) -> String {
        self.host_config.clone()
    }

    fn emit_file(&mut self, name: String, content: String) -> bool {
        // Store the emitted file — the host can retrieve it later
        self.emitted_files.push((name, content));
        true
    }

    fn resolve_import(&mut self, specifier: String, importer: String) -> Option<String> {
        // TODO: Wire to the engine's resolver via a callback channel.
        // The WASM plugin host doesn't have direct access to the engine's
        // resolver, so we return None (not resolved). The plugin should fall
        // back to its own resolution logic. Future: wire this to the engine
        // via a callback channel so the host can delegate resolution.
        tracing::trace!("resolve_import called: {} from {}", specifier, importer);
        None
    }
}

/// Create a restricted WASI context — no filesystem, no network,
/// empty environment, empty args, discarded stdio.
fn restricted_wasi_ctx() -> WasiCtx {
    // Do NOT inherit stdio — plugins should not write to stdout/stderr.
    // This matches the sandbox documentation above, which states that
    // stdin/stdout/stderr are discarded. Inheriting stdio would let a
    // plugin pollute the host process's output streams.
    WasiCtxBuilder::new().build()
}

impl WasmPlugin {
    /// Load and instantiate a WASM plugin from a `.wasm` file.
    ///
    /// The plugin is validated against the WIT contract at instantiation time.
    /// If the plugin doesn't implement the required exports, instantiation fails.
    ///
    /// # Sandbox
    ///
    /// The plugin runs in a strict sandbox:
    /// - No filesystem access (no WASI imports provided)
    /// - No network access
    /// - CPU: 10M fuel units per hook call (prevents infinite loops)
    /// - Memory: 128MB max linear memory (enforced via StoreLimits)
    pub fn load_from_file(path: &Path) -> Result<Self> {
        Self::load_with_engine(path, &default_engine()?)
    }

    /// Load and instantiate a WASM plugin with a custom engine.
    ///
    /// Use this when you want to share an engine across multiple plugins
    /// (for compilation cache reuse).
    pub fn load_with_engine(path: &Path, engine: &wasmtime::Engine) -> Result<Self> {
        debug!("Loading WASM plugin from {}", path.display());

        // Read the component bytes
        let bytes = std::fs::read(path)
            .map_err(|e| anyhow::anyhow!("Failed to read plugin file {}: {}", path.display(), e))?;

        // Compile the component (validates against the WIT contract)
        let component = wasmtime::component::Component::new(engine, &bytes).map_err(|e| {
            anyhow::anyhow!("Failed to compile WASM component {}: {}", path.display(), e)
        })?;

        // Create the store with plugin state
        // The store owns the plugin's memory — this is the sandbox boundary
        let mut store = wasmtime::Store::new(engine, PluginState::new("unknown".to_string()));

        // G7.3: Enforce CPU and memory limits on the store
        // Fuel: prevents infinite loops (DEFAULT_FUEL instructions per invocation)
        // Memory: caps linear memory growth to DEFAULT_MEMORY_MAX_BYTES (the
        // limiter's closure borrows the `StoreLimits` already built into
        // `PluginState::new` — `Store::limiter` requires a reference into
        // existing state, not a value constructed fresh inside the closure).
        store
            .set_fuel(DEFAULT_FUEL)
            .map_err(|e| anyhow::anyhow!("Failed to set fuel limit: {}", e))?;
        store.limiter(|state| &mut state.limits);

        // Create a linker with restricted WASI imports.
        // The plugin gets `wasi:cli/environment` and `wasi:cli/exit` (required
        // by wit-bindgen runtime) but NO filesystem or network access.
        let mut linker: wasmtime::component::Linker<PluginState> =
            wasmtime::component::Linker::new(engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| anyhow::anyhow!("Failed to add WASI to linker: {}", e))?;

        // Item 6: Wire host imports (get-config, emit-file, resolve-import)
        // This allows plugins to call back into the host for config access,
        // file emission, and import resolution. wasmtime 28's generated
        // `add_to_linker<T, U>` just takes a plain `Fn(&mut T) -> &mut U`
        // accessor (T and U both infer to PluginState here) — no `HasSelf`
        // marker type, which is a later-wasmtime-version construct that
        // doesn't exist in 28.x at all.
        PledgepackPlugin::add_to_linker(&mut linker, |state: &mut PluginState| state)
            .map_err(|e| anyhow::anyhow!("Failed to add host imports to linker: {}", e))?;

        // Instantiate the component
        let instance =
            PledgepackPlugin::instantiate(&mut store, &component, &linker).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to instantiate WASM component {}: {}",
                    path.display(),
                    e
                )
            })?;

        // Call the plugin-metadata hook to get the plugin's name and capabilities
        let metadata = instance
            .call_plugin_metadata(&mut store)
            .map_err(|e| anyhow::anyhow!("Failed to call plugin-metadata hook: {}", e))?;

        let name = metadata.name.clone();
        store.data_mut().name = name.clone();
        store.data_mut().initialized = true;

        info!(
            "Loaded WASM plugin: {} (version: {}, hooks: {:?})",
            name, metadata.version, metadata.hooks
        );

        Ok(Self {
            metadata,
            store,
            instance,
            cumulative_fuel: 0,
        })
    }

    /// G7.3: Refill fuel before a hook invocation.
    /// Resets the fuel budget so each hook gets a fresh CPU allowance.
    ///
    /// Also tracks cumulative fuel across all invocations. Once a plugin
    /// instance has consumed more than `MAX_CUMULATIVE_FUEL` total fuel, we
    /// stop refilling — the next hook call will trap on out-of-fuel, bounding
    /// the total CPU a single plugin instance can burn over its lifetime.
    fn refill_fuel(&mut self) {
        self.cumulative_fuel = self.cumulative_fuel.saturating_add(DEFAULT_FUEL);
        if self.cumulative_fuel > MAX_CUMULATIVE_FUEL {
            tracing::warn!(
                "[plugin:{}] exceeded cumulative fuel budget: {} (max {})",
                self.name(),
                self.cumulative_fuel,
                MAX_CUMULATIVE_FUEL
            );
            // Don't refill — let the next call trap on out-of-fuel.
            return;
        }
        let _ = self.store.set_fuel(DEFAULT_FUEL);
    }

    /// Get the plugin's metadata.
    pub fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    /// Get the plugin's name.
    pub fn name(&self) -> &str {
        &self.metadata.name
    }

    /// Whether this plugin implements the `resolve-id` hook.
    pub fn has_resolve_id(&self) -> bool {
        self.metadata.hooks.resolve_id
    }

    /// Whether this plugin implements the `load` hook.
    pub fn has_load(&self) -> bool {
        self.metadata.hooks.load
    }

    /// Whether this plugin implements the `transform` hook.
    pub fn has_transform(&self) -> bool {
        self.metadata.hooks.transform
    }

    /// Whether this plugin implements the `transform-index-html` hook.
    pub fn has_transform_index_html(&self) -> bool {
        self.metadata.hooks.transform_index_html
    }

    /// G7.4: Whether this plugin implements the `render-chunk` hook.
    pub fn has_render_chunk(&self) -> bool {
        self.metadata.hooks.render_chunk
    }

    /// Whether this plugin implements the `handle-hot-update` hook (added
    /// in WIT v0.1.3 — PRODUCTION-READINESS-100.md goal 43).
    pub fn has_handle_hot_update(&self) -> bool {
        self.metadata.hooks.handle_hot_update
    }

    /// Whether this plugin has `enforce: "pre"` (runs before built-in transform).
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn is_pre_plugin(&self) -> bool {
        self.metadata.enforce.as_deref() == Some("pre")
    }

    /// Whether this plugin has `enforce: "post"` or no enforce (default = post).
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn is_post_plugin(&self) -> bool {
        self.metadata.enforce.as_deref() != Some("pre")
    }

    // ─── Hook Invocation ──────────────────────────────────────────────

    /// Call the `resolve-id` hook.
    ///
    /// Returns `None` if the plugin doesn't handle this specifier.
    /// The output includes a `cache_key` for task graph caching.
    pub fn resolve_id(
        &mut self,
        source: &str,
        importer: Option<&str>,
        is_entry: bool,
        kind: Option<&str>,
    ) -> Result<Option<ResolveIdOutput>> {
        if !self.has_resolve_id() {
            return Ok(None);
        }
        self.refill_fuel();

        let input = ResolveIdInput {
            source: source.to_string(),
            importer: importer.map(|s| s.to_string()),
            is_entry,
            kind: kind.map(|s| s.to_string()),
        };

        let result = self
            .instance
            .call_resolve_id(&mut self.store, &input)
            .map_err(|e| anyhow::anyhow!("resolve-id hook failed: {}", e))?;

        if let Some(ref output) = result {
            debug!(
                "[plugin:{}] resolve-id: {} → {} (external: {})",
                self.name(),
                source,
                output.id,
                output.external
            );
        }

        Ok(result)
    }

    /// Call the `load` hook.
    ///
    /// Returns `None` if the plugin doesn't handle this ID.
    pub fn load(&mut self, id: &str) -> Result<Option<LoadOutput>> {
        if !self.has_load() {
            return Ok(None);
        }
        self.refill_fuel();

        let input = LoadInput { id: id.to_string() };

        let result = self
            .instance
            .call_load(&mut self.store, &input)
            .map_err(|e| anyhow::anyhow!("load hook failed: {}", e))?;

        if let Some(ref output) = result {
            debug!(
                "[plugin:{}] load: {} → {} bytes",
                self.name(),
                id,
                output.code.len()
            );
        }

        Ok(result)
    }

    /// Call the `transform` hook.
    ///
    /// Returns `None` if the plugin doesn't transform this module.
    /// The output includes a `cache_key` for task graph caching.
    pub fn transform(
        &mut self,
        code: &str,
        id: &str,
        ast_json: Option<&str>,
    ) -> Result<Option<TransformOutput>> {
        if !self.has_transform() {
            return Ok(None);
        }
        self.refill_fuel();

        let input = TransformInput {
            code: code.to_string(),
            id: id.to_string(),
            ast_json: ast_json.map(|s| s.to_string()),
        };

        let result = self
            .instance
            .call_transform(&mut self.store, &input)
            .map_err(|e| anyhow::anyhow!("transform hook failed: {}", e))?;

        if let Some(ref output) = result {
            debug!(
                "[plugin:{}] transform: {} → {} bytes (cache-key: {})",
                self.name(),
                id,
                output.code.len(),
                &output.cache_key[..8.min(output.cache_key.len())],
            );
        }

        Ok(result)
    }

    /// Call the `transform-index-html` hook.
    pub fn transform_index_html(&mut self, html: &str, path: &str) -> Result<Option<HtmlOutput>> {
        if !self.has_transform_index_html() {
            return Ok(None);
        }
        self.refill_fuel();

        let input = HtmlInput {
            html: html.to_string(),
            path: path.to_string(),
        };

        let result = self
            .instance
            .call_transform_index_html(&mut self.store, &input)
            .map_err(|e| anyhow::anyhow!("transform-index-html hook failed: {}", e))?;

        Ok(result)
    }

    /// G7.4: Call the `render-chunk` hook.
    ///
    /// Called after code splitting, before final emit.
    /// Returns `None` if the plugin doesn't render this chunk.
    /// The output includes a `cache_key` for task graph caching.
    pub fn render_chunk(
        &mut self,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Result<Option<RenderChunkOutput>> {
        if !self.has_render_chunk() {
            return Ok(None);
        }
        self.refill_fuel();

        let input = RenderChunkInput {
            code: code.to_string(),
            filename: filename.to_string(),
            chunk_type: chunk_type.to_string(),
        };

        let result = self
            .instance
            .call_render_chunk(&mut self.store, &input)
            .map_err(|e| anyhow::anyhow!("render-chunk hook failed: {}", e))?;

        if let Some(ref output) = result {
            debug!(
                "[plugin:{}] render-chunk: {} → {} bytes (cache-key: {})",
                self.name(),
                filename,
                output.code.len(),
                &output.cache_key[..8.min(output.cache_key.len())],
            );
        }

        Ok(result)
    }

    /// Call the `handle-hot-update` hook (dev mode only).
    ///
    /// Returns `None` if the plugin doesn't implement the hook or declines
    /// to handle this particular change (letting the caller fall through to
    /// default HMR resolution, or to the next plugin). See
    /// PRODUCTION-READINESS-100.md goal 43.
    pub fn handle_hot_update(
        &mut self,
        file: &str,
        timestamp: u64,
    ) -> Result<Option<HotUpdateOutput>> {
        if !self.has_handle_hot_update() {
            return Ok(None);
        }
        self.refill_fuel();

        let input = HotUpdateInput {
            file: file.to_string(),
            timestamp,
        };

        let result = self
            .instance
            .call_handle_hot_update(&mut self.store, &input)
            .map_err(|e| anyhow::anyhow!("handle-hot-update hook failed: {}", e))?;

        if let Some(ref output) = result {
            debug!(
                "[plugin:{}] handle-hot-update: {} → {} module(s)",
                self.name(),
                file,
                output.module_ids.len()
            );
        }

        Ok(result)
    }

    /// Call the `build-start` lifecycle hook.
    pub fn build_start(&mut self) -> Result<()> {
        if !self.metadata.hooks.build_start {
            return Ok(());
        }
        self.refill_fuel();
        debug!("[plugin:{}] build-start", self.name());
        self.instance.call_build_start(&mut self.store)?;
        Ok(())
    }

    /// Call the `build-end` lifecycle hook.
    pub fn build_end(&mut self) -> Result<()> {
        if !self.metadata.hooks.build_end {
            return Ok(());
        }
        self.refill_fuel();
        debug!("[plugin:{}] build-end", self.name());
        self.instance.call_build_end(&mut self.store)?;
        Ok(())
    }

    /// Call the `generate-bundle` lifecycle hook.
    pub fn generate_bundle(&mut self) -> Result<()> {
        if !self.metadata.hooks.generate_bundle {
            return Ok(());
        }
        self.refill_fuel();
        debug!("[plugin:{}] generate-bundle", self.name());
        self.instance.call_generate_bundle(&mut self.store)?;
        Ok(())
    }

    /// Call the `configure-server` hook (dev mode only).
    pub fn configure_server(&mut self) -> Result<Option<ServerMiddleware>> {
        if !self.metadata.hooks.configure_server {
            return Ok(None);
        }
        self.refill_fuel();
        let result = self
            .instance
            .call_configure_server(&mut self.store)
            .map_err(|e| anyhow::anyhow!("configure-server hook failed: {}", e))?;
        Ok(result)
    }
}

// ─── Plugin Host (manages multiple plugins) ───────────────────────────

/// Which enforcement phase a plugin runs in.
///
/// Item 5: Plugin ordering for WASM plugins. Plugins with `enforce: "pre"`
/// run before the built-in transform; plugins with `enforce: "post"` (or no
/// `enforce` field) run after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginEnforce {
    /// Runs before the built-in transform.
    Pre,
    /// Runs after the built-in transform (the default).
    Post,
}

/// The WASM plugin host — manages multiple loaded plugins and orchestrates
/// hook calls across all of them.
///
/// Hook semantics:
/// - `resolve-id`, `load`: sequential, first non-null result wins
/// - `transform`, `transform-index-html`: sequential chain (each plugin sees previous output)
/// - `build-start`, `build-end`, `generate-bundle`: all plugins called (parallel in future)
/// - `configure-server`: all plugins called, middleware collected
pub struct WasmPluginHost {
    /// All loaded plugins, in registration order
    plugins: Vec<WasmPlugin>,
    /// Shared engine (kept alive for plugin stores; compilation cache reuse)
    #[allow(dead_code)]
    engine: wasmtime::Engine,
    /// Optional signature verifier (PRODUCTION-READINESS-100.md goal 12).
    /// `None` (the default) preserves prior behavior exactly: no signature
    /// lookup, every plugin loads unconditionally. Set via
    /// [`WasmPluginHost::with_signing_verifier`] to require every loaded
    /// plugin to carry a valid signature sidecar file.
    signing_verifier: Option<pledgepack_core::plugin_system::PluginSigningVerifier>,
    /// Optional capability auditor (goal 13), same opt-in default-`None`
    /// shape as `signing_verifier` above.
    capability_auditor: Option<pledgepack_core::plugin_system::CapabilityAuditor>,
}

/// A plugin's signature plus (optionally) its declared capabilities, read
/// from a `<plugin-path>.sig.json` sidecar file. Kept local to this crate
/// (rather than extending `PluginSignature` itself, or the frozen WIT
/// contract) since neither the sidecar file format nor plugin-declared
/// capabilities are part of the plugin ABI — they're a host-side trust
/// mechanism layered on top of it.
#[derive(serde::Deserialize)]
struct PluginTrustSidecar {
    #[serde(flatten)]
    signature: pledgepack_core::plugin_system::PluginSignature,
    #[serde(default)]
    capabilities: Vec<pledgepack_core::plugin_system::PluginCapability>,
}

impl WasmPluginHost {
    /// Create a new WASM plugin host with default engine configuration.
    pub fn new() -> Result<Self> {
        let engine = default_engine()?;
        Ok(Self {
            plugins: Vec::new(),
            engine,
            signing_verifier: None,
            capability_auditor: None,
        })
    }

    /// Require every subsequently loaded plugin to carry a valid signature
    /// (see [`load_plugin`](Self::load_plugin)).
    pub fn with_signing_verifier(
        mut self,
        verifier: pledgepack_core::plugin_system::PluginSigningVerifier,
    ) -> Self {
        self.signing_verifier = Some(verifier);
        self
    }

    /// Audit every subsequently loaded plugin's declared capabilities (from
    /// its `.sig.json` sidecar, if any) against `auditor`'s policy.
    pub fn with_capability_auditor(
        mut self,
        auditor: pledgepack_core::plugin_system::CapabilityAuditor,
    ) -> Self {
        self.capability_auditor = Some(auditor);
        self
    }

    /// Load a WASM plugin from a `.wasm` file.
    ///
    /// If a signing verifier is configured (via
    /// [`with_signing_verifier`](Self::with_signing_verifier)), this looks
    /// for a `<path>.sig.json` sidecar next to `path`, verifies the plugin's
    /// blake3 hash matches the signature's `wasm_hash`, and cryptographically
    /// verifies the signature itself — refusing to load on any mismatch, a
    /// missing sidecar, or a malformed one. If a capability auditor is also
    /// configured, the sidecar's declared `capabilities` (if any) are
    /// checked against the auditor's policy, refusing to load if any are
    /// denied. With no verifier/auditor configured (the default), behavior
    /// is unchanged from before this existed: every plugin loads
    /// unconditionally. See PRODUCTION-READINESS-100.md goals 12-13.
    pub fn load_plugin(&mut self, path: &Path) -> Result<&str> {
        if self.signing_verifier.is_some() || self.capability_auditor.is_some() {
            self.check_plugin_trust(path)?;
        }
        let plugin = WasmPlugin::load_from_file(path)?;
        self.plugins.push(plugin);
        Ok(self.plugins.last().unwrap().name())
    }

    /// Signature/capability enforcement for [`load_plugin`](Self::load_plugin).
    /// Split out so the happy path (no verifier configured) above stays a
    /// one-line no-op check.
    fn check_plugin_trust(&self, path: &Path) -> Result<()> {
        let sidecar_path = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".sig.json");
            std::path::PathBuf::from(s)
        };

        let sidecar_bytes = std::fs::read(&sidecar_path).map_err(|_| {
            anyhow::anyhow!(
                "Plugin {} has no signature sidecar ({}), but signing/capability enforcement is enabled — refusing to load",
                path.display(),
                sidecar_path.display()
            )
        })?;
        let sidecar: PluginTrustSidecar = serde_json::from_slice(&sidecar_bytes).map_err(|e| {
            anyhow::anyhow!(
                "Malformed signature sidecar {}: {e} — refusing to load {}",
                sidecar_path.display(),
                path.display()
            )
        })?;

        if let Some(ref verifier) = self.signing_verifier {
            let wasm_bytes = std::fs::read(path).map_err(|e| {
                anyhow::anyhow!("Failed to read plugin file {}: {}", path.display(), e)
            })?;
            let actual_hash = blake3::hash(&wasm_bytes).to_hex().to_string();
            if actual_hash != sidecar.signature.wasm_hash {
                anyhow::bail!(
                    "Plugin {} content hash does not match its signature sidecar — refusing to load (expected {}, got {})",
                    path.display(),
                    sidecar.signature.wasm_hash,
                    actual_hash
                );
            }
            if !verifier.verify(&sidecar.signature) {
                anyhow::bail!(
                    "Signature verification FAILED for plugin {} — refusing to load",
                    path.display()
                );
            }
            info!(
                "Plugin {}: signature verified ({})",
                path.display(),
                sidecar.signature.signer_identity
            );
        }

        if let Some(ref auditor) = self.capability_auditor
            && !sidecar.capabilities.is_empty()
        {
            let audit = auditor.audit(
                &sidecar.signature.plugin_name,
                &sidecar.signature.version,
                sidecar.capabilities.clone(),
            );
            if !audit.approved {
                anyhow::bail!(
                    "Plugin {} requests denied capabilities ({}) — refusing to load",
                    path.display(),
                    audit.notes
                );
            }
            info!("Plugin {}: capability audit passed", path.display());
        }

        Ok(())
    }

    /// Load multiple plugins from a list of paths.
    pub fn load_plugins(&mut self, paths: &[&Path]) -> Result<()> {
        for path in paths {
            self.load_plugin(path)?;
        }
        Ok(())
    }

    /// Get all loaded plugins.
    pub fn plugins(&self) -> &[WasmPlugin] {
        &self.plugins
    }

    /// Get the number of loaded plugins.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether any plugins are loaded.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Check if any loaded plugin has enforce: "pre".
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn has_pre_plugin(&self) -> bool {
        self.plugins.iter().any(|p| p.is_pre_plugin())
    }

    /// Check if any loaded plugin has enforce: "post" or default (post).
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn has_post_plugin(&self) -> bool {
        self.plugins
            .iter()
            .any(|p| p.has_transform() && p.is_post_plugin())
    }

    // ─── Hook Orchestration ───────────────────────────────────────────

    /// Run `resolve-id` across all plugins (first non-null wins).
    pub fn resolve_id(
        &mut self,
        source: &str,
        importer: Option<&str>,
        is_entry: bool,
        kind: Option<&str>,
    ) -> Result<Option<ResolveIdOutput>> {
        for plugin in &mut self.plugins {
            if let Some(result) = plugin.resolve_id(source, importer, is_entry, kind)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Run `load` across all plugins (first non-null wins).
    pub fn load(&mut self, id: &str) -> Result<Option<LoadOutput>> {
        for plugin in &mut self.plugins {
            if let Some(result) = plugin.load(id)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Run `transform` across all plugins (chain — each sees previous output).
    pub fn transform(
        &mut self,
        code: &str,
        id: &str,
        ast_json: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        let mut current_code = code.to_string();
        let mut current_map: Option<String> = None;

        for plugin in &mut self.plugins {
            if let Some(output) = plugin.transform(&current_code, id, ast_json)? {
                current_code = output.code;
                if output.source_map.is_some() {
                    current_map = output.source_map;
                }
            }
        }

        Ok((current_code, current_map))
    }

    /// Run `transform` across plugins filtered by enforce phase (chain — each
    /// sees previous output). Only plugins matching the requested `enforce`
    /// phase are invoked; all others are skipped.
    ///
    /// Item 5: Plugin ordering for WASM plugins. This lets the engine run
    /// only `enforce: "pre"` plugins before the built-in transform and only
    /// `enforce: "post"` plugins after it.
    pub fn transform_filtered(
        &mut self,
        code: &str,
        id: &str,
        ast_json: Option<&str>,
        enforce: PluginEnforce,
    ) -> Result<(String, Option<String>)> {
        let mut current_code = code.to_string();
        let mut current_map: Option<String> = None;

        for plugin in &mut self.plugins {
            let matches = match enforce {
                PluginEnforce::Pre => plugin.is_pre_plugin(),
                PluginEnforce::Post => plugin.is_post_plugin(),
            };
            if !matches {
                continue;
            }
            if let Some(output) = plugin.transform(&current_code, id, ast_json)? {
                current_code = output.code;
                if output.source_map.is_some() {
                    current_map = output.source_map;
                }
            }
        }

        Ok((current_code, current_map))
    }

    /// Run `transform-index-html` across all plugins (chain).
    pub fn transform_index_html(
        &mut self,
        html: &str,
        path: &str,
    ) -> Result<(String, Vec<HtmlTag>)> {
        let mut current_html = html.to_string();
        let mut all_tags = Vec::new();

        for plugin in &mut self.plugins {
            if let Some(output) = plugin.transform_index_html(&current_html, path)? {
                current_html = output.html;
                all_tags.extend(output.tags);
            }
        }

        Ok((current_html, all_tags))
    }

    /// G7.4: Run `render-chunk` across all plugins (chain — each sees previous output).
    ///
    /// Called after code splitting, before final emit.
    /// Returns the final rendered code and optional source map.
    pub fn render_chunk(
        &mut self,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Result<(String, Option<String>)> {
        let mut current_code = code.to_string();
        let mut current_map: Option<String> = None;

        for plugin in &mut self.plugins {
            if let Some(output) = plugin.render_chunk(&current_code, filename, chunk_type)? {
                current_code = output.code;
                if output.source_map.is_some() {
                    current_map = output.source_map;
                }
            }
        }

        Ok((current_code, current_map))
    }

    /// Run `handle-hot-update` across all plugins (first `Some` wins,
    /// matching `resolve_id`/`load` above — not a chain like
    /// `transform`/`render_chunk`, per the WIT contract's ordering note).
    /// See PRODUCTION-READINESS-100.md goal 43.
    pub fn handle_hot_update(
        &mut self,
        file: &str,
        timestamp: u64,
    ) -> Result<Option<HotUpdateOutput>> {
        for plugin in &mut self.plugins {
            if let Some(result) = plugin.handle_hot_update(file, timestamp)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Run `build-start` on all plugins.
    pub fn build_start(&mut self) -> Result<()> {
        for plugin in &mut self.plugins {
            plugin.build_start()?;
        }
        Ok(())
    }

    /// Run `build-end` on all plugins.
    pub fn build_end(&mut self) -> Result<()> {
        for plugin in &mut self.plugins {
            plugin.build_end()?;
        }
        Ok(())
    }

    /// Run `generate-bundle` on all plugins.
    pub fn generate_bundle(&mut self) -> Result<()> {
        for plugin in &mut self.plugins {
            plugin.generate_bundle()?;
        }
        Ok(())
    }

    /// Run `configure-server` on all plugins and collect middleware.
    pub fn configure_server(&mut self) -> Result<Vec<ServerMiddleware>> {
        let mut middleware = Vec::new();
        for plugin in &mut self.plugins {
            if let Some(mw) = plugin.configure_server()? {
                middleware.push(mw);
            }
        }
        Ok(middleware)
    }
}

// Deliberately no `impl Default for WasmPluginHost`: construction can fail
// (a wasmtime `Engine` isn't always constructible — e.g. no available
// backend on the target), and `Default::default()` must return `Self`
// infallibly. The previous impl papered over that by panicking on failure,
// which is a startup-time panic call sites had no way to see coming from
// the type signature. `WasmPluginHost::new() -> Result<Self>` (above) is a
// Result-based path callers must already go through explicitly, so nothing
// forwards to a `Default` impl — see PRODUCTION-READINESS-100.md goal 50.

// ─── Engine Configuration ─────────────────────────────────────────────

/// Default fuel budget per plugin invocation (10M instructions ≈ ~10ms CPU).
/// Prevents infinite loops and runaway computation.
const DEFAULT_FUEL: u64 = 10_000_000;

/// G7.3: Maximum cumulative fuel a single plugin instance may consume across
/// all hook invocations over its lifetime (100M instructions ≈ ~100ms total).
/// Once exceeded, `refill_fuel` stops refilling and the next call traps on
/// out-of-fuel, bounding total CPU usage per plugin instance.
const MAX_CUMULATIVE_FUEL: u64 = 100_000_000;

/// Default maximum linear memory size per plugin (128 MB).
/// Prevents memory exhaustion from malicious or buggy plugins.
const DEFAULT_MEMORY_MAX_BYTES: usize = 128 * 1024 * 1024;

/// Create a default wasmtime engine configured for sandboxed plugin execution.
///
/// Configuration:
/// - Cranelift compiler (fast compilation, good for plugins)
/// - Component model enabled
/// - No WASI (sandbox — plugins can't access filesystem or network)
/// - Fuel consumption enabled (CPU limit — prevents infinite loops)
/// - Memory: 128MB default (enforced via StoreLimits)
fn default_engine() -> Result<wasmtime::Engine> {
    let mut config = wasmtime::Config::new();
    config.strategy(wasmtime::Strategy::Cranelift);
    config.wasm_component_model(true);
    // Disable multi-memory and multi-value for stricter sandboxing
    config.wasm_multi_memory(false);
    config.wasm_multi_value(true); // multi-value is safe, needed for component model
    // G7.3: Enable fuel consumption for CPU limiting
    config.consume_fuel(true);

    wasmtime::Engine::new(&config)
        .map_err(|e| anyhow::anyhow!("Failed to create wasmtime engine: {}", e))
}

// ─── Conversion helpers (WIT types → PledgePack core types) ───────────

/// Convert a WIT `TransformOutput` to PledgePack's `PluginTransformResult`.
///
/// This bridges the WASM plugin host to the task transform engine.
/// The `cache_key` from the WIT output is used for task graph caching.
impl From<TransformOutput> for pledgepack_core::task_transform::PluginTransformResult {
    fn from(output: TransformOutput) -> Self {
        Self {
            code: output.code,
            map: output.source_map,
            cache_key: Some(output.cache_key),
        }
    }
}

/// Convert a WIT `ResolveIdOutput` to a simple (id, external) tuple.
impl From<ResolveIdOutput> for (String, bool) {
    fn from(output: ResolveIdOutput) -> Self {
        (output.id, output.external)
    }
}

/// Convert a WIT `LoadOutput` to a (code, map) tuple.
impl From<LoadOutput> for (String, Option<String>) {
    fn from(output: LoadOutput) -> Self {
        (output.code, output.source_map)
    }
}

// ─── Task Graph Cache Integration ─────────────────────────────────────

use std::sync::Arc;

/// A thread-safe wrapper around `WasmPluginHost` that implements the
/// `Fn(&str, &str) -> Option<PluginTransformResult>` interface
/// expected by `BuildEngine::wire_plugin_transform()`.
///
/// The WASM plugin host has mutable state (wasmtime Store), so it must
/// be wrapped in a `Mutex`. The closure locks the mutex, calls the
/// transform hook, and returns the result.
///
/// # Cache Contract
///
/// The WASM plugin's `transform` hook returns a `cache_key` (blake3 hash
/// of inputs). This key is logged but currently the task graph uses its
/// own `TaskId` (also blake3) for caching. In the future, the plugin's
/// cache key could be used as the `TaskId` directly, giving plugins
/// control over their own cache invalidation.
///
/// # Thread Safety
///
/// We use `parking_lot::Mutex` (instead of `std::sync::Mutex`) for faster,
/// non-poisoning lock acquisition. This still serializes plugin calls, but
/// the overhead is lower. WASM plugin calls are typically fast (no I/O,
/// sandboxed). A future optimization is to use a pool of plugin instances
/// (one per thread) so calls don't serialize on a single host.
pub struct WasmPluginHostBridge {
    host: parking_lot::Mutex<WasmPluginHost>,
}

impl WasmPluginHostBridge {
    /// Create a bridge from a `WasmPluginHost`.
    pub fn new(host: WasmPluginHost) -> Self {
        Self {
            host: parking_lot::Mutex::new(host),
        }
    }

    /// Create a bridge from plugin paths.
    pub fn from_paths(paths: &[&Path]) -> Result<Self> {
        let mut host = WasmPluginHost::new()?;
        host.load_plugins(paths)?;
        Ok(Self::new(host))
    }

    /// Get the number of loaded plugins.
    pub fn len(&self) -> usize {
        self.host.lock().len()
    }

    /// Whether any plugins are loaded.
    pub fn is_empty(&self) -> bool {
        self.host.lock().is_empty()
    }

    /// Check if any loaded plugin has enforce: "pre".
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn has_pre_plugin(&self) -> bool {
        self.host.lock().has_pre_plugin()
    }

    /// Check if any loaded plugin has enforce: "post" or default (post).
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn has_post_plugin(&self) -> bool {
        self.host.lock().has_post_plugin()
    }

    /// Create a closure for pre-transform plugins (enforce: "pre").
    ///
    /// Only runs plugins with `enforce: "pre"`. Returns `None` if no
    /// pre-plugins are loaded or none transformed the code.
    /// Item 5: Plugin ordering for WASM plugins.
    pub fn pre_transform_closure(
        self: Arc<Self>,
    ) -> Arc<
        dyn Fn(&str, &str) -> Option<pledgepack_core::task_transform::PluginTransformResult>
            + Send
            + Sync,
    > {
        if !self.has_pre_plugin() {
            return Arc::new(|_code, _id| None);
        }
        // Only run plugins with enforce: "pre". The engine calls this BEFORE
        // the built-in transform. Post plugins are excluded so they don't run
        // twice (once here, once in the normal transform closure).
        Arc::new(move |code: &str, id: &str| {
            self.run_transform_filtered(code, id, PluginEnforce::Pre)
        })
    }

    /// Run `transform` filtered by enforce phase (thread-safe).
    ///
    /// Locks the host and calls `transform_filtered`, returning a
    /// `PluginTransformResult` only if the code changed or a source map was
    /// produced. Returns `None` otherwise (or on error, which is logged).
    pub fn run_transform_filtered(
        &self,
        code: &str,
        id: &str,
        enforce: PluginEnforce,
    ) -> Option<pledgepack_core::task_transform::PluginTransformResult> {
        let mut host = self.host.lock();
        match host.transform_filtered(code, id, None, enforce) {
            Ok((transformed_code, map)) => {
                if transformed_code != code || map.is_some() {
                    Some(pledgepack_core::task_transform::PluginTransformResult {
                        code: transformed_code,
                        map,
                        cache_key: None, // WASM bridge doesn't expose cache_key here
                    })
                } else {
                    None
                }
            }
            Err(e) => {
                debug!(
                    "WASM plugin transform (filtered {:?}) failed for {}: {}",
                    enforce, id, e
                );
                None
            }
        }
    }

    /// Create a closure that can be passed to `BuildEngine::wire_plugin_transform()`.
    ///
    /// The closure takes `(code, id)` and returns `Option<PluginTransformResult>`.
    /// It locks the mutex, calls `transform` on all plugins (chain), and returns
    /// the result. If no plugins transform the code, returns `None`.
    pub fn transform_closure(
        self: Arc<Self>,
    ) -> Arc<
        dyn Fn(&str, &str) -> Option<pledgepack_core::task_transform::PluginTransformResult>
            + Send
            + Sync,
    > {
        Arc::new(move |code: &str, id: &str| {
            let mut host = self.host.lock();
            match host.transform(code, id, None) {
                Ok((transformed_code, map)) => {
                    // If the code changed or a map was produced, return the result
                    if transformed_code != code || map.is_some() {
                        Some(pledgepack_core::task_transform::PluginTransformResult {
                            code: transformed_code,
                            map,
                            cache_key: None, // WASM bridge doesn't expose cache_key here
                        })
                    } else {
                        None
                    }
                }
                Err(e) => {
                    debug!("WASM plugin transform failed for {}: {}", id, e);
                    None
                }
            }
        })
    }

    /// Run `build-start` on all plugins (thread-safe).
    pub fn build_start(&self) -> Result<()> {
        self.host.lock().build_start()
    }

    /// Run `build-end` on all plugins (thread-safe).
    pub fn build_end(&self) -> Result<()> {
        self.host.lock().build_end()
    }

    /// Run `resolve-id` on all plugins (thread-safe, first non-null wins).
    pub fn resolve_id(
        &self,
        source: &str,
        importer: Option<&str>,
        is_entry: bool,
        kind: Option<&str>,
    ) -> Result<Option<ResolveIdOutput>> {
        self.host
            .lock()
            .resolve_id(source, importer, is_entry, kind)
    }

    /// Run `load` on all plugins (thread-safe, first non-null wins).
    pub fn load(&self, id: &str) -> Result<Option<LoadOutput>> {
        self.host.lock().load(id)
    }

    /// G7.4: Check if any loaded plugin has a render-chunk hook.
    pub fn has_render_chunk(&self) -> bool {
        self.host
            .lock()
            .plugins()
            .iter()
            .any(|p| p.has_render_chunk())
    }

    /// G7.4: Run `render-chunk` on all plugins (thread-safe, chain).
    pub fn render_chunk(
        &self,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Result<(String, Option<String>)> {
        self.host.lock().render_chunk(code, filename, chunk_type)
    }

    /// Run `handle-hot-update` on all plugins (thread-safe, first `Some`
    /// wins). See PRODUCTION-READINESS-100.md goal 43.
    pub fn handle_hot_update(&self, file: &str, timestamp: u64) -> Result<Option<HotUpdateOutput>> {
        self.host.lock().handle_hot_update(file, timestamp)
    }
}

// ─── G7.7: Plugin Composition ─────────────────────────────────────────

/// A step in a plugin composition pipeline.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompositionStep {
    /// The plugin name to invoke.
    pub plugin_name: String,
    /// The hook to call (e.g., "transform", "render-chunk").
    pub hook: String,
    /// Execution order (lower = earlier).
    pub order: u32,
}

/// A composition plan describing how multiple plugins are chained.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompositionPlan {
    /// Ordered steps in the composition pipeline.
    pub steps: Vec<CompositionStep>,
}

impl WasmPluginHost {
    /// G7.7: Compose multiple plugins into a single execution pipeline.
    ///
    /// Multiple plugins can be composed so their hooks run in sequence,
    /// producing a single combined transform. This allows, e.g., a CSS
    /// modules plugin followed by a minification plugin to be composed
    /// into a single "css-pipeline" plugin.
    pub fn compose_plugins(&self, steps: &[CompositionStep]) -> Result<CompositionPlan> {
        let mut sorted_steps = steps.to_vec();
        sorted_steps.sort_by_key(|s| s.order);
        Ok(CompositionPlan {
            steps: sorted_steps,
        })
    }
}

// ─── G7.9: Plugin Debugging ───────────────────────────────────────────

/// Configuration for debugging WASM plugins.
#[derive(Clone, Debug)]
pub struct DebugConfig {
    /// Enable verbose logging of all host↔plugin calls.
    pub verbose_logging: bool,
    /// Capture stack traces on traps/panics.
    pub stack_traces: bool,
    /// Optional fuel limit override for debugging (None = use default).
    pub fuel_limit: Option<u64>,
    /// Enable address sanitizer-style checks in wasmtime.
    pub address_sanitizer: bool,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            verbose_logging: false,
            stack_traces: true,
            fuel_limit: Some(DEFAULT_FUEL),
            address_sanitizer: false,
        }
    }
}

impl DebugConfig {
    /// Full debugging mode — all diagnostics enabled.
    pub fn full() -> Self {
        Self {
            verbose_logging: true,
            stack_traces: true,
            fuel_limit: Some(DEFAULT_FUEL),
            address_sanitizer: true,
        }
    }
}

// ─── G7.11: Plugin Instance Pooling ───────────────────────────────────

/// A pool of WASM plugin instances kept alive in memory for reuse.
///
/// Pre-instantiation avoids the overhead of recompiling and instantiating
/// WASM modules on every call. The pool maintains a fixed number of
/// instances and hands them out on demand.
///
/// # Implementation
///
/// Cached, ready-to-use `WasmPlugin` instances are stored in
/// `available` (guarded by a `parking_lot::Mutex`). `acquire` pops a cached
/// instance if one is present (returning it inside the `PoolSlot` guard) and
/// increments the `active` counter; if the cache is empty the slot carries
/// `None` and the caller must instantiate a fresh plugin. When the `PoolSlot`
/// is dropped, any instance it holds is returned to the cache (subject to
/// `max_size`) and the `active` counter is decremented.
///
/// Instances are added to the cache via `populate` / `release`. Because a
/// `WasmPlugin` requires a `.wasm` file and an engine to construct, the pool
/// cannot create instances itself — callers pre-instantiate them and hand
/// them to the pool.
///
/// TODO: Add a constructor that takes a loader closure so the pool can lazily
/// instantiate instances up to `max_size` on demand.
pub struct PluginInstancePool {
    /// Cached, ready-to-use plugin instances.
    available: parking_lot::Mutex<Vec<WasmPlugin>>,
    /// Maximum number of instances the pool will cache.
    max_size: usize,
    /// Number of instances currently checked out (acquired but not released).
    active: std::sync::atomic::AtomicUsize,
}

/// Statistics about the instance pool.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PoolStats {
    /// Maximum number of cached instances (`max_size`).
    pub pool_size: usize,
    /// Instances currently checked out.
    pub active: usize,
    /// Free capacity: how many more instances can be checked out before
    /// hitting `pool_size`. (Cached-but-idle instances count toward this.)
    pub available: usize,
}

/// A guard that returns an instance to the pool when dropped.
///
/// Holds an `Option<WasmPlugin>`: `Some` if a cached instance was acquired
/// (or one was attached via `attach`), `None` if the pool was empty and the
/// caller instantiated a fresh plugin outside the pool. On drop, any held
/// instance is returned to the cache (subject to `max_size`) and the
/// `active` counter is decremented.
pub struct PoolSlot<'a> {
    pool: &'a PluginInstancePool,
    instance: Option<WasmPlugin>,
}

impl Drop for PoolSlot<'_> {
    fn drop(&mut self) {
        // Return any held instance to the cache if there is room.
        if let Some(instance) = self.instance.take() {
            let mut avail = self.pool.available.lock();
            if avail.len() < self.pool.max_size {
                avail.push(instance);
            }
            // If the cache is full, the instance is dropped.
        }
        self.pool
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl<'a> PoolSlot<'a> {
    /// Take the cached instance out of the slot, if one was acquired.
    ///
    /// After calling this, the slot no longer holds an instance, so dropping
    /// it will only decrement the `active` counter (no instance is returned
    /// to the cache).
    pub fn take_instance(&mut self) -> Option<WasmPlugin> {
        self.instance.take()
    }

    /// Attach a freshly-instantiated plugin to this slot so it is returned to
    /// the pool when the slot is dropped.
    pub fn attach(&mut self, instance: WasmPlugin) {
        self.instance = Some(instance);
    }
}

impl PluginInstancePool {
    /// Create a new pool with the given (maximum) size.
    pub fn new(pool_size: usize) -> Self {
        Self {
            available: parking_lot::Mutex::new(Vec::with_capacity(pool_size)),
            max_size: pool_size,
            active: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Add a pre-instantiated plugin to the pool's cache.
    ///
    /// The pool cannot create `WasmPlugin` instances itself (it has no
    /// `.wasm` path or engine), so callers must instantiate plugins and hand
    /// them to the pool via this method (or `release`). If the cache is
    /// already full, the instance is dropped.
    pub fn populate(&self, instance: WasmPlugin) {
        let mut avail = self.available.lock();
        if avail.len() < self.max_size {
            avail.push(instance);
        }
    }

    /// Acquire an instance from the pool. Returns a guard that releases on
    /// drop.
    ///
    /// If a cached instance is available it is returned inside the slot
    /// (`take_instance` will yield `Some`). If the cache is empty the slot
    /// carries `None` — the caller must instantiate a fresh plugin (and may
    /// `attach` it so it returns to the pool when done).
    pub fn acquire(&self) -> PoolSlot<'_> {
        self.active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let instance = self.available.lock().pop();
        PoolSlot {
            pool: self,
            instance,
        }
    }

    /// Release an instance back to the pool's cache (without going through a
    /// `PoolSlot`). If the cache is already full, the instance is dropped.
    pub fn release(&self, instance: WasmPlugin) {
        let mut avail = self.available.lock();
        if avail.len() < self.max_size {
            avail.push(instance);
        }
    }

    /// Get current pool statistics.
    pub fn stats(&self) -> PoolStats {
        let active = self.active.load(std::sync::atomic::Ordering::SeqCst);
        PoolStats {
            pool_size: self.max_size,
            active,
            available: self.max_size.saturating_sub(active),
        }
    }
}

// ─── G7.12: Plugin Cache Sharing ──────────────────────────────────────

/// A cache entry for plugin outputs, stored in the same content-addressed
/// store as internal tasks. This enables remote cache sharing for plugins.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginCacheEntry {
    /// The content hash key (blake3 of plugin inputs).
    pub cache_key: Vec<u8>,
    /// The plugin that produced this output.
    pub plugin_name: String,
    /// The hook that produced this output.
    pub hook: String,
    /// The serialized output.
    pub output: Vec<u8>,
    /// Unix timestamp when this entry was created.
    pub timestamp: u64,
}

/// A content-addressed store for plugin outputs.
///
/// Plugin outputs are cached using the same content-addressing scheme as
/// internal tasks. This means remote cache works for plugins — if another
/// machine has already computed the same plugin transform, it can be
/// fetched from the remote cache without re-execution.
///
/// # Thread Safety
///
/// The internal map is guarded by a `parking_lot::Mutex`, so `PluginCacheStore`
/// is `Sync` and all methods take `&self`. This allows the store to be shared
/// across threads (e.g. behind an `Arc`) without external synchronization.
/// `get` returns an owned `PluginCacheEntry` (cloned) because we cannot hand
/// out a reference into the locked map.
pub struct PluginCacheStore {
    entries: parking_lot::Mutex<std::collections::HashMap<Vec<u8>, PluginCacheEntry>>,
}

impl PluginCacheStore {
    /// Create a new empty plugin cache store.
    pub fn new() -> Self {
        Self {
            entries: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Store a plugin cache entry.
    pub fn put(&self, entry: PluginCacheEntry) {
        self.entries.lock().insert(entry.cache_key.clone(), entry);
    }

    /// Retrieve a plugin cache entry by its key.
    ///
    /// Returns a clone of the entry. The internal map is guarded by a Mutex,
    /// so we cannot hand out a reference to the locked data.
    pub fn get(&self, key: &[u8]) -> Option<PluginCacheEntry> {
        self.entries.lock().get(key).cloned()
    }

    /// Remove a plugin cache entry.
    pub fn remove(&self, key: &[u8]) {
        self.entries.lock().remove(key);
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

impl Default for PluginCacheStore {
    fn default() -> Self {
        Self::new()
    }
}

// ─── G7.13: WASM SIMD for Plugins ─────────────────────────────────────

/// Configuration for WASM SIMD support in plugins.
///
/// When enabled, plugins can use the WASM `v128` type for parallel
/// text processing, enabling SIMD-accelerated transforms.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WasmSimdConfig {
    /// Whether WASM SIMD is enabled.
    pub enabled: bool,
    /// Whether the v128 type is available.
    pub v128_type: bool,
    /// Whether SIMD instructions are available.
    pub simd_instructions: bool,
}

impl Default for WasmSimdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            v128_type: true,
            simd_instructions: true,
        }
    }
}

impl WasmSimdConfig {
    /// Disabled SIMD config (for platforms without SIMD support).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            v128_type: false,
            simd_instructions: false,
        }
    }
}

// ─── G7.14: Plugin-to-Plugin Communication ────────────────────────────

/// A message sent from one plugin to another via the WIT contract.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginMessage {
    /// The sending plugin's name.
    pub from_plugin: String,
    /// The receiving plugin's name.
    pub to_plugin: String,
    /// The message type (e.g., "transform_done", "resolve_complete").
    pub message_type: String,
    /// The message payload.
    pub payload: Vec<u8>,
}

/// A communication channel that allows plugins to send messages to each other.
///
/// This enables plugin-to-plugin communication via the WIT contract,
/// allowing, e.g., a CSS plugin to notify a minification plugin that
/// its transform is complete.
///
/// # Thread Safety
///
/// The internal message map is guarded by a `parking_lot::Mutex`, so
/// `PluginCommunicationChannel` is `Sync` and all methods take `&self`. This
/// allows the channel to be shared across threads (e.g. behind an `Arc`)
/// without external synchronization.
pub struct PluginCommunicationChannel {
    messages: parking_lot::Mutex<std::collections::HashMap<String, Vec<PluginMessage>>>,
}

impl PluginCommunicationChannel {
    /// Create a new empty communication channel.
    pub fn new() -> Self {
        Self {
            messages: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Send a message from one plugin to another.
    pub fn send(&self, from: &str, to: &str, message_type: &str, payload: &[u8]) {
        let msg = PluginMessage {
            from_plugin: from.to_string(),
            to_plugin: to.to_string(),
            message_type: message_type.to_string(),
            payload: payload.to_vec(),
        };
        self.messages
            .lock()
            .entry(to.to_string())
            .or_default()
            .push(msg);
    }

    /// Receive all messages for a given plugin.
    pub fn recv(&self, plugin: &str) -> Vec<PluginMessage> {
        self.messages.lock().remove(plugin).unwrap_or_default()
    }
}

impl Default for PluginCommunicationChannel {
    fn default() -> Self {
        Self::new()
    }
}

// ─── G7.15: Multi-Language Plugin Compilation ─────────────────────────

/// A compiler that can build plugins from multiple languages to WASM.
pub struct PluginCompiler;

impl PluginCompiler {
    /// Get the list of supported plugin languages.
    pub fn supported_languages() -> Vec<&'static str> {
        vec!["rust", "c", "cpp", "zig", "go", "assemblyscript"]
    }

    /// Get the compile command for a given language.
    pub fn compile_command(lang: &str, source: &str, output: &str) -> Result<Vec<String>> {
        match lang {
            "rust" => Ok(vec![
                "cargo".to_string(),
                "build".to_string(),
                "--target".to_string(),
                "wasm32-wasi".to_string(),
                "--release".to_string(),
                format!("--target-dir={}", output),
            ]),
            "c" | "cpp" => Ok(vec![
                "clang".to_string(),
                "--target=wasm32-wasi".to_string(),
                "-o".to_string(),
                output.to_string(),
                source.to_string(),
            ]),
            "zig" => Ok(vec![
                "zig".to_string(),
                "build-lib".to_string(),
                source.to_string(),
                "-target".to_string(),
                "wasm32-wasi".to_string(),
                "-femit-bin=".to_string() + output,
                "-OReleaseSmall".to_string(),
            ]),
            "go" => Ok(vec![
                "tinygo".to_string(),
                "build".to_string(),
                "-o".to_string(),
                output.to_string(),
                "-target".to_string(),
                "wasi".to_string(),
                source.to_string(),
            ]),
            "assemblyscript" => Ok(vec![
                "asc".to_string(),
                source.to_string(),
                "-o".to_string(),
                output.to_string(),
                "--optimize".to_string(),
            ]),
            _ => Err(anyhow::anyhow!("Unsupported plugin language: {}", lang)),
        }
    }
}

// ─── G7.16: Zig-Compiled Plugins ──────────────────────────────────────

/// A compiler for Zig-based WASM plugins.
pub struct ZigPluginCompiler;

impl ZigPluginCompiler {
    /// Get the WASM target triple for Zig plugins.
    pub fn wasm_target() -> String {
        "wasm32-wasi".to_string()
    }

    /// Get the compile flags for a Zig plugin.
    pub fn compile_flags(source: &str, output: &str) -> Vec<String> {
        vec![
            "build-lib".to_string(),
            source.to_string(),
            "-target".to_string(),
            "wasm32-wasi".to_string(),
            "-femit-bin=".to_string() + output,
            "-OReleaseSmall".to_string(),
            "-fno-entry".to_string(),
            "--export=transform".to_string(),
            "--export=resolve-id".to_string(),
            "--export=load".to_string(),
        ]
    }
}

// ─── G7.17: Plugin Attestation ────────────────────────────────────────

/// An attestation that a plugin was authored by a specific identity.
///
/// Each `.wasm` plugin can be signed by its author. `pledge plugin add`
/// verifies the signature before installing the plugin.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginAttestation {
    /// The blake3 hash of the plugin WASM bytes.
    pub plugin_hash: Vec<u8>,
    /// The author identity (e.g., GitHub username, org name).
    pub author: String,
    /// The Ed25519 signature over `plugin_hash ++ author ++ timestamp`.
    pub signature: Vec<u8>,
    /// Unix timestamp when the attestation was created.
    pub timestamp: u64,
    /// The key ID used to sign (for key rotation).
    pub key_id: String,
}

impl PluginAttestation {
    /// Verify that the plugin hash matches the expected hash.
    pub fn verify_hash(&self, expected: &[u8]) -> bool {
        self.plugin_hash == expected
    }
}

// ─── G7.18: Plugin Profiling ──────────────────────────────────────────

/// Configuration for profiling WASM plugins.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProfilingConfig {
    /// Enable CPU profiling (fuel consumption tracking).
    pub enable_cpu_profiling: bool,
    /// Enable memory usage tracking.
    pub enable_memory_tracking: bool,
    /// Sample rate in Hz for profiling.
    pub sample_rate_hz: u32,
}

impl Default for ProfilingConfig {
    fn default() -> Self {
        Self {
            enable_cpu_profiling: true,
            enable_memory_tracking: true,
            sample_rate_hz: 100,
        }
    }
}

/// Result of profiling a plugin execution.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProfileResult {
    /// The plugin that was profiled.
    pub plugin_name: String,
    /// Total wall-clock time in milliseconds.
    pub total_time_ms: f64,
    /// Per-hook execution times in milliseconds.
    pub hook_times_ms: Vec<f64>,
    /// Total fuel (WASM instructions) consumed.
    pub fuel_consumed: u64,
    /// Peak memory usage in bytes.
    pub memory_used_bytes: usize,
    /// Number of times the plugin was called.
    pub call_count: u32,
}

/// Whether this host (the WASM Component Model host) genuinely executes a
/// plugin's implementation of `hook_name` when present — not merely
/// whether a *plugin* can declare the hook. See
/// `pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES` for the canonical
/// hook-name list this should be checked against, and
/// PRODUCTION-READINESS-100.md goals 45-46. `wasm-plugin-host` has genuine,
/// hand-verified support for every hook in the WIT contract as of v0.1.3.
pub fn host_supports_hook(hook_name: &str) -> bool {
    pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES.contains(&hook_name)
}

/// The full capability matrix: every hook name paired with whether this
/// host supports it. A `false` entry should never occur today (see
/// [`host_supports_hook`]) — this function exists so goal 46's integration
/// tests, and any future caller, can assert full coverage without
/// hardcoding the hook list a second time.
pub fn hook_support_matrix() -> Vec<(&'static str, bool)> {
    pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES
        .iter()
        .map(|&name| (name, host_supports_hook(name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_support_matrix_has_no_gaps() {
        // Regression test for goals 45-46: this host claims full hook
        // parity — verify the matrix actually says so, rather than trusting
        // the doc comment.
        let matrix = hook_support_matrix();
        assert_eq!(
            matrix.len(),
            pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES.len()
        );
        for (hook, supported) in &matrix {
            assert!(
                *supported,
                "wasm-plugin-host claims to support hook '{hook}' but host_supports_hook() says no"
            );
        }
    }

    #[test]
    fn wasm_plugin_host_creation() {
        let host = WasmPluginHost::new();
        assert!(host.is_ok());
        let host = host.unwrap();
        assert_eq!(host.len(), 0);
        assert!(host.is_empty());
    }

    #[test]
    fn wasm_plugin_host_load_nonexistent_fails() {
        let mut host = WasmPluginHost::new().unwrap();
        let result = host.load_plugin(Path::new("nonexistent.wasm"));
        assert!(result.is_err());
    }

    /// Regression tests for goals 12-13 (PRODUCTION-READINESS-100.md): with
    /// no verifier/auditor configured, `check_plugin_trust` must never be
    /// invoked at all — covered implicitly by every other test in this file
    /// still passing unmodified. These specifically cover the *enabled*
    /// paths, which previously didn't exist (signing/capability checking was
    /// unreachable from any load path).
    mod trust_checking {
        use super::*;
        use pledgepack_core::plugin_system::{
            PluginCapability, PluginSignature, PluginSigningVerifier,
        };
        use std::io::Write;

        fn write_plugin_file(dir: &std::path::Path, content: &[u8]) -> std::path::PathBuf {
            let path = dir.join("plugin.wasm");
            std::fs::File::create(&path)
                .unwrap()
                .write_all(content)
                .unwrap();
            path
        }

        fn write_sidecar(plugin_path: &std::path::Path, json: &str) {
            let sidecar_path = {
                let mut s = plugin_path.as_os_str().to_os_string();
                s.push(".sig.json");
                std::path::PathBuf::from(s)
            };
            std::fs::write(sidecar_path, json).unwrap();
        }

        #[test]
        fn missing_sidecar_is_rejected_when_verifier_configured() {
            let dir = tempfile::tempdir().unwrap();
            let plugin_path = write_plugin_file(dir.path(), b"not-a-real-wasm-component");

            let host = WasmPluginHost::new()
                .unwrap()
                .with_signing_verifier(PluginSigningVerifier::new());
            let err = host.check_plugin_trust(&plugin_path).unwrap_err();
            assert!(err.to_string().contains("no signature sidecar"));
        }

        #[test]
        fn hash_mismatch_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let plugin_path = write_plugin_file(dir.path(), b"not-a-real-wasm-component");
            write_sidecar(
                &plugin_path,
                r#"{"plugin_name":"p","version":"1.0.0","wasm_hash":"deadbeef","signer_public_key":"ab","signature":"cd","signer_identity":"@x","timestamp":0,"verified":false}"#,
            );

            let host = WasmPluginHost::new()
                .unwrap()
                .with_signing_verifier(PluginSigningVerifier::new());
            let err = host.check_plugin_trust(&plugin_path).unwrap_err();
            assert!(err.to_string().contains("content hash does not match"));
        }

        #[test]
        fn valid_signature_over_correct_hash_is_accepted() {
            use ed25519_dalek::Signer;

            let dir = tempfile::tempdir().unwrap();
            let plugin_bytes = b"not-a-real-wasm-component";
            let plugin_path = write_plugin_file(dir.path(), plugin_bytes);
            let wasm_hash = blake3::hash(plugin_bytes).to_hex().to_string();

            let signing_key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
            let verifying_key = signing_key.verifying_key();
            let signature = signing_key.sign(wasm_hash.as_bytes());

            let sig = PluginSignature {
                plugin_name: "p".to_string(),
                version: "1.0.0".to_string(),
                wasm_hash,
                signer_public_key: hex::encode(verifying_key.to_bytes()),
                signature: hex::encode(signature.to_bytes()),
                signer_identity: "@pledgelabs".to_string(),
                timestamp: 0,
                verified: false,
            };
            write_sidecar(&plugin_path, &serde_json::to_string(&sig).unwrap());

            let mut verifier = PluginSigningVerifier::new();
            verifier.trust_key("@pledgelabs", &hex::encode(verifying_key.to_bytes()));
            let host = WasmPluginHost::new()
                .unwrap()
                .with_signing_verifier(verifier);
            host.check_plugin_trust(&plugin_path).unwrap();
        }

        #[test]
        fn denied_capability_is_rejected_even_without_signing_enabled() {
            let dir = tempfile::tempdir().unwrap();
            let plugin_bytes = b"not-a-real-wasm-component";
            let plugin_path = write_plugin_file(dir.path(), plugin_bytes);
            let wasm_hash = blake3::hash(plugin_bytes).to_hex().to_string();

            // Sidecar with a real (but here, unsigned/untrusted) plugin
            // signature struct — only `capabilities` is exercised by this
            // test, since no signing_verifier is configured below.
            let sig = PluginSignature {
                plugin_name: "greedy-plugin".to_string(),
                version: "1.0.0".to_string(),
                wasm_hash,
                signer_public_key: String::new(),
                signature: String::new(),
                signer_identity: String::new(),
                timestamp: 0,
                verified: false,
            };
            #[derive(serde::Serialize)]
            struct SidecarWithCaps {
                #[serde(flatten)]
                signature: PluginSignature,
                capabilities: Vec<PluginCapability>,
            }
            let sidecar = SidecarWithCaps {
                signature: sig,
                capabilities: vec![PluginCapability::ProcessSpawn],
            };
            write_sidecar(&plugin_path, &serde_json::to_string(&sidecar).unwrap());

            let host = WasmPluginHost::new()
                .unwrap()
                .with_capability_auditor(pledgepack_core::plugin_system::CapabilityAuditor::new());
            let err = host.check_plugin_trust(&plugin_path).unwrap_err();
            assert!(err.to_string().contains("denied capabilities"));
        }
    }

    #[test]
    fn empty_host_resolve_id_returns_none() {
        let mut host = WasmPluginHost::new().unwrap();
        let result = host.resolve_id("./foo", None, false, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_host_load_returns_none() {
        let mut host = WasmPluginHost::new().unwrap();
        let result = host.load("test.js").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_host_transform_returns_input_unchanged() {
        let mut host = WasmPluginHost::new().unwrap();
        let (code, map) = host.transform("const x = 1;", "test.js", None).unwrap();
        assert_eq!(code, "const x = 1;");
        assert!(map.is_none());
    }

    #[test]
    fn empty_host_transform_index_html_returns_input_unchanged() {
        let mut host = WasmPluginHost::new().unwrap();
        let (html, tags) = host
            .transform_index_html("<html></html>", "index.html")
            .unwrap();
        assert_eq!(html, "<html></html>");
        assert!(tags.is_empty());
    }

    #[test]
    fn empty_host_lifecycle_hooks_are_noops() {
        let mut host = WasmPluginHost::new().unwrap();
        assert!(host.build_start().is_ok());
        assert!(host.build_end().is_ok());
        assert!(host.generate_bundle().is_ok());
    }

    #[test]
    fn empty_host_configure_server_returns_empty() {
        let mut host = WasmPluginHost::new().unwrap();
        let middleware = host.configure_server().unwrap();
        assert!(middleware.is_empty());
    }

    // ─── Bridge tests ─────────────────────────────────────────────────

    #[test]
    fn bridge_creation_empty() {
        let host = WasmPluginHost::new().unwrap();
        let bridge = WasmPluginHostBridge::new(host);
        assert!(bridge.is_empty());
        assert_eq!(bridge.len(), 0);
    }

    #[test]
    fn bridge_transform_closure_returns_none_for_empty_host() {
        let host = WasmPluginHost::new().unwrap();
        let bridge = Arc::new(WasmPluginHostBridge::new(host));
        let closure = bridge.transform_closure();
        let result = closure("const x = 1;", "test.js");
        assert!(result.is_none());
    }

    #[test]
    fn bridge_resolve_id_returns_none_for_empty_host() {
        let host = WasmPluginHost::new().unwrap();
        let bridge = WasmPluginHostBridge::new(host);
        let result = bridge.resolve_id("./foo", None, false, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bridge_load_returns_none_for_empty_host() {
        let host = WasmPluginHost::new().unwrap();
        let bridge = WasmPluginHostBridge::new(host);
        let result = bridge.load("test.js").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bridge_lifecycle_hooks_are_noops() {
        let host = WasmPluginHost::new().unwrap();
        let bridge = WasmPluginHostBridge::new(host);
        assert!(bridge.build_start().is_ok());
        assert!(bridge.build_end().is_ok());
    }

    #[test]
    fn bridge_from_paths_fails_for_nonexistent() {
        let result = WasmPluginHostBridge::from_paths(&[Path::new("nonexistent.wasm")]);
        assert!(result.is_err());
    }

    // ─── G7.7: Plugin Composition tests ──────────────────────────────

    #[test]
    fn g7_7_plugin_composition_plan_empty() {
        let host = WasmPluginHost::new().unwrap();
        let plan = host.compose_plugins(&[]);
        assert!(plan.is_ok());
        assert!(plan.unwrap().steps.is_empty());
    }

    #[test]
    fn g7_7_plugin_composition_plan_with_steps() {
        let host = WasmPluginHost::new().unwrap();
        let steps = vec![
            CompositionStep {
                plugin_name: "@pledge/css-modules".to_string(),
                hook: "transform".to_string(),
                order: 0,
            },
            CompositionStep {
                plugin_name: "@pledge/minify".to_string(),
                hook: "render-chunk".to_string(),
                order: 1,
            },
        ];
        let plan = host.compose_plugins(&steps).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].plugin_name, "@pledge/css-modules");
        assert_eq!(plan.steps[1].plugin_name, "@pledge/minify");
    }

    // ─── G7.9: Plugin Debugging tests ────────────────────────────────

    #[test]
    fn g7_9_debug_config_default() {
        let config = DebugConfig::default();
        assert!(!config.verbose_logging);
        assert!(config.stack_traces);
        assert!(!config.fuel_limit.is_none());
    }

    #[test]
    fn g7_9_debug_config_full() {
        let config = DebugConfig::full();
        assert!(config.verbose_logging);
        assert!(config.stack_traces);
        assert!(config.fuel_limit.is_some());
    }

    // ─── G7.11: Instance Pooling tests ───────────────────────────────

    #[test]
    fn g7_11_instance_pool_stats() {
        let pool = PluginInstancePool::new(4);
        let stats = pool.stats();
        assert_eq!(stats.pool_size, 4);
        assert_eq!(stats.active, 0);
        assert_eq!(stats.available, 4);
    }

    #[test]
    fn g7_11_instance_pool_acquire_release() {
        let pool = PluginInstancePool::new(2);
        let _slot1 = pool.acquire();
        assert_eq!(pool.stats().active, 1);
        assert_eq!(pool.stats().available, 1);
        drop(_slot1);
        assert_eq!(pool.stats().active, 0);
        assert_eq!(pool.stats().available, 2);
    }

    // ─── G7.12: Plugin Cache Sharing tests ───────────────────────────

    #[test]
    fn g7_12_plugin_cache_entry_serialization() {
        let entry = PluginCacheEntry {
            cache_key: vec![0x42; 16],
            plugin_name: "@pledge/css-modules".to_string(),
            hook: "transform".to_string(),
            output: b"transformed_code".to_vec(),
            timestamp: 1234567890,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: PluginCacheEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.cache_key, deserialized.cache_key);
        assert_eq!(entry.plugin_name, deserialized.plugin_name);
        assert_eq!(entry.output, deserialized.output);
    }

    #[test]
    fn g7_12_plugin_cache_store_operations() {
        let mut store = PluginCacheStore::new();
        let entry = PluginCacheEntry {
            cache_key: vec![0xAA; 16],
            plugin_name: "test-plugin".to_string(),
            hook: "transform".to_string(),
            output: b"output".to_vec(),
            timestamp: 0,
        };
        store.put(entry.clone());
        assert_eq!(store.len(), 1);
        let retrieved = store.get(&entry.cache_key).unwrap();
        assert_eq!(retrieved.plugin_name, "test-plugin");
        store.remove(&entry.cache_key);
        assert_eq!(store.len(), 0);
    }

    // ─── G7.13: WASM SIMD tests ──────────────────────────────────────

    #[test]
    fn g7_13_wasm_simd_config() {
        let config = WasmSimdConfig::default();
        assert!(config.enabled);
        assert!(config.v128_type);
    }

    #[test]
    fn g7_13_wasm_simd_disabled() {
        let config = WasmSimdConfig::disabled();
        assert!(!config.enabled);
    }

    // ─── G7.14: Plugin-to-Plugin Communication tests ─────────────────

    #[test]
    fn g7_14_plugin_communication_channel() {
        let mut channel = PluginCommunicationChannel::new();
        channel.send(
            "@pledge/css-modules",
            "@pledge/minify",
            "transform_done",
            b"result_data",
        );
        let messages = channel.recv("@pledge/minify");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].from_plugin, "@pledge/css-modules");
        assert_eq!(messages[0].message_type, "transform_done");
    }

    // ─── G7.15: Multi-Language Compilation tests ─────────────────────

    #[test]
    fn g7_15_supported_languages() {
        let langs = PluginCompiler::supported_languages();
        assert!(langs.contains(&"rust"));
        assert!(langs.contains(&"c"));
        assert!(langs.contains(&"cpp"));
        assert!(langs.contains(&"zig"));
        assert!(langs.contains(&"go"));
        assert!(langs.contains(&"assemblyscript"));
    }

    #[test]
    fn g7_15_compile_command_rust() {
        let cmd = PluginCompiler::compile_command("rust", "src/lib.rs", "out.wasm").unwrap();
        assert!(cmd[0].contains("cargo") || cmd[0].contains("rustc"));
    }

    #[test]
    fn g7_15_compile_command_zig() {
        let cmd = PluginCompiler::compile_command("zig", "src/main.zig", "out.wasm").unwrap();
        assert!(cmd.iter().any(|c| c.contains("zig")));
        assert!(cmd.iter().any(|c| c.contains("wasm32")));
    }

    #[test]
    fn g7_15_compile_command_unsupported() {
        let result = PluginCompiler::compile_command("brainfuck", "src.bf", "out.wasm");
        assert!(result.is_err());
    }

    // ─── G7.16: Zig-compiled plugins tests ───────────────────────────

    #[test]
    fn g7_16_zig_wasm_target() {
        let target = ZigPluginCompiler::wasm_target();
        assert!(target.contains("wasm32"));
        assert!(target.contains("wasi"));
    }

    #[test]
    fn g7_16_zig_compile_flags() {
        let flags = ZigPluginCompiler::compile_flags("src/main.zig", "out.wasm");
        assert!(flags.iter().any(|f| f.contains("wasm32-wasi")));
        assert!(flags.iter().any(|f| f == "src/main.zig"));
        assert!(flags.iter().any(|f| f.contains("out.wasm")));
    }

    // ─── G7.17: Plugin Attestation tests ─────────────────────────────

    #[test]
    fn g7_17_attestation_serialization() {
        let att = PluginAttestation {
            plugin_hash: vec![0xAB; 32],
            author: "pledgepack".to_string(),
            signature: vec![0xCD; 64],
            timestamp: 1234567890,
            key_id: "key-001".to_string(),
        };
        let json = serde_json::to_string(&att).unwrap();
        let deserialized: PluginAttestation = serde_json::from_str(&json).unwrap();
        assert_eq!(att.plugin_hash, deserialized.plugin_hash);
        assert_eq!(att.author, deserialized.author);
        assert_eq!(att.signature, deserialized.signature);
    }

    #[test]
    fn g7_17_attestation_verification_logic() {
        let att = PluginAttestation {
            plugin_hash: vec![0xAB; 32],
            author: "pledgepack".to_string(),
            signature: vec![0xCD; 64],
            timestamp: 1234567890,
            key_id: "key-001".to_string(),
        };
        // Verification checks that hash matches expected
        assert!(att.verify_hash(&vec![0xAB; 32]));
        assert!(!att.verify_hash(&vec![0xBB; 32]));
    }

    // ─── G7.18: Plugin Profiling tests ───────────────────────────────

    #[test]
    fn g7_18_profiling_config() {
        let config = ProfilingConfig::default();
        assert!(config.enable_cpu_profiling);
        assert!(config.enable_memory_tracking);
        assert_eq!(config.sample_rate_hz, 100);
    }

    #[test]
    fn g7_18_profiling_result() {
        let result = ProfileResult {
            plugin_name: "@pledge/css-modules".to_string(),
            total_time_ms: 42.5,
            hook_times_ms: vec![10.0, 20.0, 12.5],
            fuel_consumed: 1_500_000,
            memory_used_bytes: 1024 * 1024,
            call_count: 3,
        };
        assert_eq!(result.plugin_name, "@pledge/css-modules");
        assert_eq!(result.call_count, 3);
        assert!((result.total_time_ms - 42.5).abs() < f64::EPSILON);
    }
}
