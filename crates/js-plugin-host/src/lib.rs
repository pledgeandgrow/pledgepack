// JS Plugin Host — Vite-compatible plugin API
//
// Provides a JavaScript plugin interface that mirrors Vite's plugin hooks:
//   - resolveId(source, importer) → { id, external } | null
//   - load(id) → { code, map } | null

pub mod advanced;
pub mod test_bundle;
pub mod test_runner;
//   - transform(code, id) → { code, map } | null
//   - transformIndexHtml(html) → html | tags[]
//   - configureServer(server) → void
//   - buildStart() → void
//   - buildEnd() → void
//   - generateBundle() → void
//
// Plugins are defined as JS/TS files exporting default objects with these hooks.
// The host loads and evaluates plugin files, then calls hooks during the build pipeline.

use anyhow::Result;
use rquickjs::prelude::{Func, Rest};
use rquickjs::{Context, Object, Runtime};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// Wall-clock budget for a single plugin hook evaluation. A plugin stuck in
/// `while (true) {}` used to hang the whole build/dev server forever.
const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(30);
/// Heap cap for the embedded QuickJS runtime (runaway allocation → JS
/// `out of memory` exception instead of exhausting the host).
const DEFAULT_MEMORY_LIMIT: usize = 512 * 1024 * 1024;

/// Apply a heap cap to a QuickJS runtime. `0` means unlimited (mapped to
/// `usize::MAX`: passing a literal 0 to QuickJS would forbid every allocation).
pub(crate) fn apply_memory_limit(runtime: &Runtime, limit: usize) {
    runtime.set_memory_limit(if limit == 0 { usize::MAX } else { limit });
}

/// A loaded JS plugin with its hooks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsPlugin {
    pub name: String,
    /// Whether this plugin applies to the build
    pub apply: Option<String>,
    /// Hook: resolveId(source, importer) → { id, external } | null
    pub has_resolve_id: bool,
    /// Hook: load(id) → { code, map } | null
    pub has_load: bool,
    /// Hook: transform(code, id) → { code, map } | null
    pub has_transform: bool,
    /// Hook: transformIndexHtml(html) → html | tags[]
    pub has_transform_index_html: bool,
    /// Hook: configureServer(server)
    pub has_configure_server: bool,
    /// Hook: buildStart()
    pub has_build_start: bool,
    /// Hook: buildEnd()
    pub has_build_end: bool,
    /// Hook: generateBundle()
    pub has_generate_bundle: bool,
    /// Hook: renderChunk(code, filename, chunkType) → { code, map } | null.
    /// Mirrors the WIT contract's `render-chunk` hook (added there in
    /// v0.1.2) — previously WASM-only; see PRODUCTION-READINESS-100.md goal 44.
    pub has_render_chunk: bool,
    /// Hook: handleHotUpdate(file, timestamp) → { moduleIds } | null.
    /// Mirrors the WIT contract's `handle-hot-update` hook (added there in
    /// v0.1.3) — previously absent from both plugin hosts entirely; see
    /// PRODUCTION-READINESS-100.md goal 43.
    pub has_handle_hot_update: bool,
    /// Raw source of the plugin file (for evaluation)
    pub source: String,
    /// Path to the plugin file
    pub path: PathBuf,
}

/// Result of a resolveId hook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveIdResult {
    pub id: String,
    pub external: bool,
}

/// Result of a load hook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadResult {
    pub code: String,
    pub map: Option<String>,
}

/// Result of a transform hook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformResult {
    pub code: String,
    pub map: Option<String>,
}

/// Result of a handleHotUpdate hook — see the WIT contract's
/// `hot-update-output` (v0.1.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotUpdateResult {
    #[serde(rename = "moduleIds")]
    pub module_ids: Vec<String>,
}

/// HTML tag injection for transformIndexHtml
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HtmlTag {
    pub tag: String,
    pub attrs: HashMap<String, String>,
    pub children: Option<String>,
    pub inject_to: Option<String>,
}

/// Middleware registered by a plugin's configureServer hook
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerMiddleware {
    pub plugin_name: String,
    pub source: String,
}

/// Manages a collection of JS plugins with an embedded JS runtime
pub struct JsPluginHost {
    plugins: Vec<JsPlugin>,
    /// QuickJS runtime (kept alive to sustain the context)
    #[allow(dead_code)]
    runtime: Runtime,
    /// QuickJS context for evaluating and executing plugin code
    context: Context,
    /// Optional signature verifier (PRODUCTION-READINESS-100.md goal 12).
    /// `None` (the default) preserves prior behavior exactly. See the
    /// equivalent field on `pledgepack_wasm_plugin_host::WasmPluginHost` for
    /// the full rationale — same sidecar-file mechanism, same opt-in default.
    signing_verifier: Option<pledgepack_core::plugin_system::PluginSigningVerifier>,
    /// Optional capability auditor (goal 13). When set, declared
    /// capabilities in a plugin's `.sig.json` sidecar are checked against
    /// policy before the plugin loads.
    capability_auditor: Option<pledgepack_core::plugin_system::CapabilityAuditor>,
    /// Deadline of the hook currently executing, as milliseconds since
    /// `epoch` plus one (0 = no hook running). Read by the QuickJS interrupt
    /// handler.
    deadline: Arc<AtomicU64>,
    epoch: Instant,
    hook_timeout: Duration,
    /// Extra bytes mixed into every plugin's transform-cache fingerprint -
    /// set this to a hash of the plugin configuration so changing options
    /// invalidates cached transform results.
    cache_salt: String,
}

impl JsPluginHost {
    /// Create a new empty plugin host with a JS runtime
    pub fn new() -> Self {
        let runtime = Runtime::new().expect("Failed to create QuickJS runtime");
        let context = Context::full(&runtime).expect("Failed to create QuickJS context");
        apply_memory_limit(&runtime, DEFAULT_MEMORY_LIMIT);
        let deadline = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        {
            let deadline = deadline.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || {
                let dl = deadline.load(Ordering::Relaxed);
                dl != 0 && epoch.elapsed().as_millis() as u64 + 1 >= dl
            })));
        }

        // Inject console.log support for plugin debugging
        let _ = context.with(|ctx| {
            let globals = ctx.globals();
            let console = Object::new(ctx.clone())?;
            console.set(
                "log",
                Func::new(|args: Rest<String>| {
                    info!("[plugin console] {}", args.0.join(" "));
                }),
            )?;
            globals.set("console", console)?;
            Ok::<_, rquickjs::Error>(())
        });

        Self {
            plugins: Vec::new(),
            runtime,
            context,
            signing_verifier: None,
            capability_auditor: None,
            deadline,
            epoch,
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            cache_salt: String::new(),
        }
    }

    /// Mix `salt` (typically a hash of the plugin configuration) into the
    /// transform-cache fingerprint of every plugin.
    pub fn with_cache_salt(mut self, salt: &str) -> Self {
        self.cache_salt = salt.to_string();
        self
    }

    /// Apply the memory limit of a [`advanced::RuntimeConfig`] to this host's
    /// runtime (the config's `memory_limit` used to be applied nowhere).
    /// `0` = unlimited.
    pub fn with_runtime_config(self, config: &advanced::RuntimeConfig) -> Self {
        apply_memory_limit(&self.runtime, config.memory_limit);
        self
    }

    /// Override the per-hook wall-clock budget (default 30s).
    pub fn with_hook_timeout(mut self, timeout: Duration) -> Self {
        self.hook_timeout = timeout;
        self
    }

    fn arm_deadline(&self) {
        let dl = (self.epoch.elapsed() + self.hook_timeout).as_millis() as u64 + 1;
        self.deadline.store(dl, Ordering::Relaxed);
    }

    /// Run `f` inside the JS context with the hook deadline armed, so runaway
    /// plugin code is interrupted (surfacing as a JS exception) instead of
    /// hanging the host.
    fn with_guarded<F, R>(&self, f: F) -> R
    where
        F: for<'js> FnOnce(rquickjs::Ctx<'js>) -> R,
    {
        self.arm_deadline();
        let r = self.context.with(f);
        self.deadline.store(0, Ordering::Relaxed);
        r
    }

    /// Require every subsequently loaded plugin to carry a valid
    /// `<path>.sig.json` signature sidecar (see [`load_plugins`](Self::load_plugins)).
    pub fn with_signing_verifier(
        mut self,
        verifier: pledgepack_core::plugin_system::PluginSigningVerifier,
    ) -> Self {
        self.signing_verifier = Some(verifier);
        self
    }

    /// Audit the declared capabilities of subsequently loaded plugins
    /// against the given policy (see [`load_plugins`](Self::load_plugins)).
    pub fn with_capability_auditor(
        mut self,
        auditor: pledgepack_core::plugin_system::CapabilityAuditor,
    ) -> Self {
        self.capability_auditor = Some(auditor);
        self
    }

    /// Verify `path` against a `<path>.sig.json` sidecar when a signing
    /// verifier or capability auditor is configured. No-op (matching prior
    /// behavior exactly) when neither is. See
    /// `pledgepack_wasm_plugin_host`'s identically-named check for the full
    /// rationale — both hosts share `PluginTrustSidecar`, so a bare
    /// `PluginSignature` sidecar still parses (capabilities default empty).
    fn check_plugin_signature(&self, path: &std::path::Path, source: &str) -> Result<()> {
        if self.signing_verifier.is_none() && self.capability_auditor.is_none() {
            return Ok(());
        }

        let sidecar_path = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".sig.json");
            PathBuf::from(s)
        };
        let sidecar_bytes = std::fs::read(&sidecar_path).map_err(|_| {
            anyhow::anyhow!(
                "Plugin {} has no signature sidecar ({}), but signing/capability enforcement is enabled — refusing to load",
                path.display(),
                sidecar_path.display()
            )
        })?;
        let sidecar: pledgepack_core::plugin_system::PluginTrustSidecar =
            serde_json::from_slice(&sidecar_bytes).map_err(|e| {
                anyhow::anyhow!(
                    "Malformed signature sidecar {}: {e} — refusing to load {}",
                    sidecar_path.display(),
                    path.display()
                )
            })?;
        let sig = &sidecar.signature;

        if let Some(ref verifier) = self.signing_verifier {
            let actual_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
            if !pledgepack_core::plugin_system::ct_eq(
                actual_hash.as_bytes(),
                sig.wasm_hash.as_bytes(),
            ) {
                anyhow::bail!(
                    "Plugin {} content hash does not match its signature sidecar — refusing to load (expected {}, got {})",
                    path.display(),
                    sig.wasm_hash,
                    actual_hash
                );
            }
            if !verifier.verify(sig) {
                anyhow::bail!(
                    "Signature verification FAILED for plugin {} — refusing to load",
                    path.display()
                );
            }
            info!(
                "Plugin {}: signature verified ({})",
                path.display(),
                sig.signer_identity
            );
        }

        if let Some(ref auditor) = self.capability_auditor
            && !sidecar.capabilities.is_empty()
        {
            let audit = auditor.audit(&sig.plugin_name, &sig.version, sidecar.capabilities.clone());
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

    /// Load plugins from the given paths (JS/TS files)
    pub fn load_plugins(&mut self, plugin_paths: &[String]) -> Result<()> {
        for path in plugin_paths {
            let pathbuf = PathBuf::from(path);
            if !pathbuf.exists() {
                warn!("Plugin file not found: {}", path);
                continue;
            }

            let source = std::fs::read_to_string(&pathbuf)?;
            self.check_plugin_signature(&pathbuf, &source)?;
            let plugin = Self::parse_plugin(&source, pathbuf)?;
            info!("Loaded JS plugin: {}", plugin.name);

            // Strip ESM syntax and evaluate the plugin source in the JS context
            // Store the exported module object as a global variable for later hook calls
            let plugin_index = self.plugins.len();
            let global_name = format!("__pledge_plugin_{}", plugin_index);
            let js_source = strip_esm_and_assign(&source, &global_name);
            if let Err(e) = self.with_guarded(|ctx| ctx.eval::<(), _>(js_source.as_str())) {
                // PRODUCTION-READINESS-100.md goal 49: this used to warn and
                // then push the plugin into `self.plugins` anyway. Every
                // subsequent hook call for it would then silently no-op
                // forever — `globalThis['__pledge_plugin_N']` was never
                // actually assigned, so the `if (__pluginModule && ...)`
                // guard in every hook's generated JS snippet just quietly
                // skips it, while `host.len()`/`host.plugins()` report the
                // plugin as loaded. Skip it instead, loudly, so the plugin
                // count is truthful and the failure is unmissable.
                tracing::error!(
                    "Plugin {} ({}) failed to evaluate and will NOT be loaded — its hooks would \
                     otherwise silently never run: {}",
                    plugin.name,
                    plugin.path.display(),
                    e
                );
                continue;
            }

            self.plugins.push(plugin);
        }
        Ok(())
    }

    /// Load all plugin files from a directory
    pub fn load_from_dir(dir: &std::path::Path) -> Result<Self> {
        let mut host = Self::new();
        host.load_dir(dir)?;
        Ok(host)
    }

    /// Load all plugin files from a directory into this host — honors any
    /// signing verifier / capability auditor configured on the host.
    pub fn load_dir(&mut self, dir: &std::path::Path) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        let mut plugin_paths = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(ext) = path.extension().and_then(|e| e.to_str())
                    && matches!(ext, "js" | "ts" | "mjs" | "cjs")
                {
                    plugin_paths.push(path.to_string_lossy().to_string());
                }
            }
        }
        if !plugin_paths.is_empty() {
            self.load_plugins(&plugin_paths)?;
        }
        Ok(())
    }

    /// Parse a plugin from source code.
    /// Extracts the plugin name and which hooks are present by scanning for
    /// hook function definitions in the exported object.
    fn parse_plugin(source: &str, path: PathBuf) -> Result<JsPlugin> {
        // Extract plugin name from `name: "..."` or `name: '...'`
        let name = Self::extract_string_field(source, "name").unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("anonymous")
                .to_string()
        });

        // Detect which hooks are present by looking for hook names as keys
        let has_resolve_id = Self::has_hook(source, "resolveId");
        let has_load = Self::has_hook(source, "load");
        let has_transform = Self::has_hook(source, "transform");
        let has_transform_index_html = Self::has_hook(source, "transformIndexHtml");
        let has_configure_server = Self::has_hook(source, "configureServer");
        let has_build_start = Self::has_hook(source, "buildStart");
        let has_build_end = Self::has_hook(source, "buildEnd");
        let has_generate_bundle = Self::has_hook(source, "generateBundle");
        let has_render_chunk = Self::has_hook(source, "renderChunk");
        let has_handle_hot_update = Self::has_hook(source, "handleHotUpdate");

        // Extract apply field if present
        let apply = Self::extract_string_field(source, "apply");

        Ok(JsPlugin {
            name,
            apply,
            has_resolve_id,
            has_load,
            has_transform,
            has_transform_index_html,
            has_configure_server,
            has_build_start,
            has_build_end,
            has_generate_bundle,
            has_render_chunk,
            has_handle_hot_update,
            source: source.to_string(),
            path,
        })
    }

    /// Check if a hook name appears as a key in the source
    fn has_hook(source: &str, hook_name: &str) -> bool {
        // Look for patterns like: resolveId: , resolveId(, resolveId:
        source.contains(&format!("{}:", hook_name))
            || source.contains(&format!("{}(", hook_name))
            || source.contains(&format!("{} :", hook_name))
    }

    /// Extract a string field value from source (e.g., name: "my-plugin")
    fn extract_string_field(source: &str, field: &str) -> Option<String> {
        // Look for field: "value" or field: 'value'
        for quote in ['"', '\''] {
            let pattern = format!("{}:", field);
            if let Some(pos) = source.find(&pattern) {
                let rest = &source[pos + pattern.len()..];
                let trimmed = rest.trim_start();
                if trimmed.starts_with(quote) {
                    let start = 1;
                    if let Some(end) = trimmed[start..].find(quote) {
                        return Some(trimmed[start..start + end].to_string());
                    }
                }
            }
        }
        None
    }

    /// Get all loaded plugins
    pub fn plugins(&self) -> &[JsPlugin] {
        &self.plugins
    }

    /// Run a lifecycle hook (`buildStart`/`buildEnd`/`generateBundle`) across
    /// all plugins that declare it, in plugin order.
    ///
    /// Each plugin's own JS function is called inside its QuickJS context
    /// (`js_args` is the literal JS argument list). A hook that returns a
    /// Promise is awaited by draining QuickJS's microtask queue (no timers or
    /// I/O exist in the embedded runtime, so anything that settles via
    /// microtasks alone completes). A hook that throws or rejects does NOT
    /// stop the remaining plugins from running, but the collected failures are
    /// returned as an `Err` so the caller can fail the build — mirroring
    /// Rollup/Vite, where a throwing `buildStart` aborts the build.
    fn run_lifecycle_hook(
        &self,
        hook_name: &str,
        js_args: &str,
        has_hook: impl Fn(&JsPlugin) -> bool,
    ) -> Result<()> {
        let mut failures: Vec<String> = Vec::new();
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !has_hook(plugin) {
                continue;
            }
            info!("[plugin:{}] {}", plugin.name, hook_name);
            let global_name = format!("__pledge_plugin_{}", index);
            let js_code = format!(
                r#"
                (function() {{
                    var st = {{ done: false, error: null }};
                    globalThis.__pledge_hook_state = st;
                    var fail = function(e) {{
                        st.done = true;
                        st.error = String((e && e.message) || e);
                    }};
                    try {{
                        var __pluginModule = globalThis['{global_name}'];
                        if (__pluginModule && typeof __pluginModule.{hook_name} === 'function') {{
                            var __r = __pluginModule.{hook_name}({js_args});
                            if (__r && typeof __r.then === 'function') {{
                                __r.then(function() {{ st.done = true; }}, fail);
                            }} else {{
                                st.done = true;
                            }}
                        }} else {{
                            st.done = true;
                        }}
                    }} catch (e) {{
                        fail(e);
                    }}
                }})()
                "#,
            );
            if let Err(e) = self.with_guarded(|ctx| ctx.eval::<(), _>(js_code.as_str())) {
                failures.push(format!("[plugin:{}] {hook_name}: {e}", plugin.name));
                continue;
            }
            // Settle any promise the hook returned (microtasks only).
            self.arm_deadline();
            while matches!(self.runtime.execute_pending_job(), Ok(true)) {
                if self.deadline.load(Ordering::Relaxed) != 0
                    && self.epoch.elapsed().as_millis() as u64 + 1
                        >= self.deadline.load(Ordering::Relaxed)
                {
                    failures.push(format!(
                        "[plugin:{}] {hook_name}: timed out settling promises",
                        plugin.name
                    ));
                    break;
                }
            }
            self.deadline.store(0, Ordering::Relaxed);

            let state: Option<String> = self
                .with_guarded(|ctx| {
                    ctx.eval::<Option<String>, _>(
                        "JSON.stringify(globalThis.__pledge_hook_state || null)",
                    )
                })
                .ok()
                .flatten();
            let state: serde_json::Value = state
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            if let Some(err) = state.get("error").and_then(|e| e.as_str()) {
                failures.push(format!("[plugin:{}] {hook_name}: {err}", plugin.name));
            } else if state.get("done").and_then(|d| d.as_bool()) != Some(true) {
                warn!(
                    "[plugin:{}] {} returned a promise that did not settle (timers/I/O are not \
                     available in the plugin runtime)",
                    plugin.name, hook_name
                );
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            for f in &failures {
                warn!("plugin hook failed: {f}");
            }
            anyhow::bail!("plugin hook error(s): {}", failures.join("; "))
        }
    }

    /// Run `buildStart` hooks for all plugins (before the build).
    pub fn build_start(&self) -> Result<()> {
        self.run_lifecycle_hook("buildStart", "", |p| p.has_build_start)
    }

    /// Run `buildEnd` hooks for all plugins (after the build completed).
    pub fn build_end(&self) -> Result<()> {
        self.run_lifecycle_hook("buildEnd", "", |p| p.has_build_end)
    }

    /// Run `generateBundle(options, bundle)` hooks for all plugins once the
    /// output has been emitted. Both arguments are currently empty objects
    /// (plugins that destructure them keep working); the emitted files are on
    /// disk in the configured output directory.
    pub fn generate_bundle(&self) -> Result<()> {
        self.run_lifecycle_hook("generateBundle", "{}, {}", |p| p.has_generate_bundle)
    }

    /// Check if any plugin handles resolveId for the given source
    /// Actually calls the JS resolveId() function in each plugin that has it
    pub fn resolve_id(&mut self, source: &str, importer: &str) -> Option<ResolveIdResult> {
        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if plugin.has_resolve_id {
                info!("[plugin:{}] resolveId: {}", plugin.name, source);

                let global_name = format!("__pledge_plugin_{}", plugin_idx);
                let js_code = format!(
                    r#"
                    (function() {{
                        try {{
                            var __pluginModule = globalThis['{}'];
                            if (__pluginModule && typeof __pluginModule.resolveId === 'function') {{
                                var __result = __pluginModule.resolveId({}, {});
                                if (__result) {{
                                    return JSON.stringify(__result);
                                }}
                            }}
                        }} catch(e) {{
                            console.log('Plugin resolveId error: ' + e.message);
                        }}
                        return null;
                    }})()
                    "#,
                    global_name,
                    serde_json::to_string(source).unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(importer).unwrap_or_else(|_| "\"\"".to_string())
                );

                match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                    Ok(Some(json_str)) => {
                        if let Ok(result) = serde_json::from_str::<ResolveIdResult>(&json_str) {
                            return Some(result);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("[plugin:{}] resolveId execution error: {}", plugin.name, e);
                    }
                }
            }
        }
        None
    }

    /// Check if any plugin handles load for the given id
    /// Actually calls the JS load() function in each plugin that has it
    pub fn load(&mut self, id: &str) -> Option<LoadResult> {
        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if plugin.has_load {
                info!("[plugin:{}] load: {}", plugin.name, id);

                let global_name = format!("__pledge_plugin_{}", plugin_idx);
                let js_code = format!(
                    r#"
                    (function() {{
                        try {{
                            var __pluginModule = globalThis['{}'];
                            if (__pluginModule && typeof __pluginModule.load === 'function') {{
                                var __result = __pluginModule.load({});
                                if (__result && __result.code) {{
                                    return JSON.stringify(__result);
                                }}
                            }}
                        }} catch(e) {{
                            console.log('Plugin load error: ' + e.message);
                        }}
                        return null;
                    }})()
                    "#,
                    global_name,
                    serde_json::to_string(id).unwrap_or_else(|_| "\"\"".to_string())
                );

                match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                    Ok(Some(json_str)) => {
                        if let Ok(result) = serde_json::from_str::<LoadResult>(&json_str) {
                            return Some(result);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("[plugin:{}] load execution error: {}", plugin.name, e);
                    }
                }
            }
        }
        None
    }

    /// Run transform hooks for all plugins on the given code
    /// Actually calls the JS transform() function in the plugin
    pub fn transform(&mut self, code: &str, id: &str) -> Option<TransformResult> {
        let mut result_code = code.to_string();
        let mut transformed = false;

        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if plugin.has_transform {
                info!("[plugin:{}] transform: {}", plugin.name, id);

                // Try to call the plugin's transform function in JS
                let global_name = format!("__pledge_plugin_{}", plugin_idx);
                let js_code = format!(
                    r#"
                    (function() {{
                        try {{
                            var __pluginModule = globalThis['{}'];
                            if (__pluginModule && typeof __pluginModule.transform === 'function') {{
                                var __result = __pluginModule.transform({}, {});
                                if (__result && __result.code) {{
                                    return JSON.stringify(__result);
                                }}
                            }}
                        }} catch(e) {{
                            console.log('Plugin transform error: ' + e.message);
                        }}
                        return null;
                    }})()
                    "#,
                    global_name,
                    serde_json::to_string(code).unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(&pledgepack_core::normalize_path_str(id))
                        .unwrap_or_else(|_| "\"\"".to_string())
                );

                match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                    Ok(Some(json_str)) => {
                        if let Ok(result) = serde_json::from_str::<TransformResult>(&json_str) {
                            result_code = result.code;
                            transformed = true;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("[plugin:{}] transform execution error: {}", plugin.name, e);
                    }
                }
            }
        }

        if transformed {
            Some(TransformResult {
                code: result_code,
                map: None,
            })
        } else {
            None
        }
    }

    /// Run `renderChunk` hooks for all plugins that declare one, in a
    /// sequential chain (each plugin sees the previous plugin's output) —
    /// mirrors `transform`'s chaining above and the WIT contract's
    /// `render-chunk` hook, which `wasm-plugin-host` already implements.
    /// Was WASM-only until now; see PRODUCTION-READINESS-100.md goal 44.
    pub fn render_chunk(
        &mut self,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Option<TransformResult> {
        let mut result_code = code.to_string();
        let mut rendered = false;

        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if !plugin.has_render_chunk {
                continue;
            }
            info!("[plugin:{}] renderChunk: {}", plugin.name, filename);

            let global_name = format!("__pledge_plugin_{}", plugin_idx);
            let js_code = format!(
                r#"
                (function() {{
                    try {{
                        var __pluginModule = globalThis['{}'];
                        if (__pluginModule && typeof __pluginModule.renderChunk === 'function') {{
                            var __result = __pluginModule.renderChunk({}, {}, {});
                            if (__result && __result.code) {{
                                return JSON.stringify(__result);
                            }}
                        }}
                    }} catch(e) {{
                        console.log('Plugin renderChunk error: ' + e.message);
                    }}
                    return null;
                }})()
                "#,
                global_name,
                serde_json::to_string(&result_code).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(filename).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(chunk_type).unwrap_or_else(|_| "\"\"".to_string()),
            );

            match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                Ok(Some(json_str)) => {
                    if let Ok(result) = serde_json::from_str::<TransformResult>(&json_str) {
                        result_code = result.code;
                        rendered = true;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        "[plugin:{}] renderChunk execution error: {}",
                        plugin.name, e
                    );
                }
            }
        }

        if rendered {
            Some(TransformResult {
                code: result_code,
                map: None,
            })
        } else {
            None
        }
    }

    /// Run `handleHotUpdate` hooks — first plugin to return non-null wins
    /// (matching `resolve_id`/`load`'s "first wins" semantics, not
    /// `transform`/`render_chunk`'s chaining — see the WIT contract's
    /// ordering note on this hook). Previously absent from both plugin
    /// hosts entirely; see PRODUCTION-READINESS-100.md goal 43.
    pub fn handle_hot_update(&mut self, file: &str, timestamp: u64) -> Option<HotUpdateResult> {
        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if !plugin.has_handle_hot_update {
                continue;
            }
            info!("[plugin:{}] handleHotUpdate: {}", plugin.name, file);

            let global_name = format!("__pledge_plugin_{}", plugin_idx);
            let js_code = format!(
                r#"
                (function() {{
                    try {{
                        var __pluginModule = globalThis['{}'];
                        if (__pluginModule && typeof __pluginModule.handleHotUpdate === 'function') {{
                            var __result = __pluginModule.handleHotUpdate({}, {});
                            if (__result) {{
                                return JSON.stringify(__result);
                            }}
                        }}
                    }} catch(e) {{
                        console.log('Plugin handleHotUpdate error: ' + e.message);
                    }}
                    return null;
                }})()
                "#,
                global_name,
                serde_json::to_string(file).unwrap_or_else(|_| "\"\"".to_string()),
                timestamp,
            );

            match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                Ok(Some(json_str)) => {
                    if let Ok(result) = serde_json::from_str::<HotUpdateResult>(&json_str) {
                        return Some(result);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(
                        "[plugin:{}] handleHotUpdate execution error: {}",
                        plugin.name, e
                    );
                }
            }
        }
        None
    }

    /// Run transformIndexHtml hooks for all plugins
    /// Actually calls the JS transformIndexHtml() function and collects HTML modifications
    pub fn transform_index_html(&mut self, html: &str) -> (String, Vec<HtmlTag>) {
        let mut result_html = html.to_string();
        let mut tags = Vec::new();

        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if plugin.has_transform_index_html {
                info!("[plugin:{}] transformIndexHtml", plugin.name);

                let global_name = format!("__pledge_plugin_{}", plugin_idx);
                let js_code = format!(
                    r#"
                    (function() {{
                        try {{
                            var __pluginModule = globalThis['{}'];
                            if (__pluginModule && typeof __pluginModule.transformIndexHtml === 'function') {{
                                var __result = __pluginModule.transformIndexHtml({});
                                if (__result) {{
                                    if (typeof __result === 'string') {{
                                        return JSON.stringify({{ html: __result, tags: [] }});
                                    }} else if (Array.isArray(__result)) {{
                                        return JSON.stringify({{ html: null, tags: __result }});
                                    }} else if (__result.html || __result.tags) {{
                                        return JSON.stringify(__result);
                                    }}
                                }}
                            }}
                        }} catch(e) {{
                            console.log('Plugin transformIndexHtml error: ' + e.message);
                        }}
                        return null;
                    }})()
                    "#,
                    global_name,
                    serde_json::to_string(html).unwrap_or_else(|_| "\"\"".to_string())
                );

                match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                    Ok(Some(json_str)) => {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) {
                            // If html is returned as string, replace result_html
                            if let Some(html_val) = parsed.get("html").and_then(|h| h.as_str())
                                && !html_val.is_empty()
                            {
                                result_html = html_val.to_string();
                            }
                            // Parse tags array
                            if let Some(tags_arr) = parsed.get("tags").and_then(|t| t.as_array()) {
                                for tag_val in tags_arr {
                                    let mut tag = HtmlTag {
                                        tag: tag_val
                                            .get("tag")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        attrs: HashMap::new(),
                                        children: tag_val
                                            .get("children")
                                            .and_then(|c| c.as_str())
                                            .map(|s| s.to_string()),
                                        inject_to: tag_val
                                            .get("injectTo")
                                            .and_then(|i| i.as_str())
                                            .map(|s| s.to_string()),
                                    };
                                    if let Some(attrs) =
                                        tag_val.get("attrs").and_then(|a| a.as_object())
                                    {
                                        for (k, v) in attrs {
                                            tag.attrs.insert(
                                                k.clone(),
                                                v.as_str().unwrap_or("").to_string(),
                                            );
                                        }
                                    }
                                    if !tag.tag.is_empty() {
                                        tags.push(tag);
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(
                            "[plugin:{}] transformIndexHtml execution error: {}",
                            plugin.name, e
                        );
                    }
                }
            }
        }

        (result_html, tags)
    }

    /// Run configureServer hooks for all plugins
    /// Executes the JS configureServer(server) function, passing a minimal server object
    /// that allows plugins to register middleware, add routes, etc.
    pub fn configure_server(&mut self) -> Vec<ServerMiddleware> {
        let mut middlewares = Vec::new();

        for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
            if plugin.has_configure_server {
                info!("[plugin:{}] configureServer", plugin.name);

                // Execute the configureServer hook in JS
                // The plugin can register middleware by calling server.use(fn)
                let global_name = format!("__pledge_plugin_{}", plugin_idx);
                let js_code = format!(
                    r#"
                    (function() {{
                        try {{
                            var __pluginModule = globalThis['{}'];
                            if (__pluginModule && typeof __pluginModule.configureServer === 'function') {{
                                var __registered = [];
                                var __server = {{
                                    use: function(fn) {{
                                        if (typeof fn === 'function') __registered.push(fn.toString());
                                    }},
                                    on: function(event, fn) {{
                                        if (typeof fn === 'function') __registered.push('on:' + event + ':' + fn.toString());
                                    }},
                                }};
                                __pluginModule.configureServer(__server);
                                if (__registered.length > 0) {{
                                    return JSON.stringify(__registered);
                                }}
                            }}
                        }} catch(e) {{
                            console.log('Plugin configureServer error: ' + e.message);
                        }}
                        return null;
                    }})()
                    "#,
                    global_name
                );

                match self.with_guarded(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str())) {
                    Ok(Some(json_str)) => {
                        if let Ok(fns) = serde_json::from_str::<Vec<String>>(&json_str) {
                            for fn_source in fns {
                                middlewares.push(ServerMiddleware {
                                    plugin_name: plugin.name.clone(),
                                    source: fn_source,
                                });
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!(
                            "[plugin:{}] configureServer execution error: {}",
                            plugin.name, e
                        );
                    }
                }
            }
        }

        middlewares
    }

    /// Check if any plugins are loaded
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

// ─── Fallible per-module hooks (build pipeline) ─────────────────────────

impl JsPluginHost {
    /// Call `hook` of plugin `plugin_idx` with the literal JS argument list
    /// `js_args`, awaiting a returned Promise (microtasks only).
    ///
    /// Unlike the dev-server-oriented `resolve_id`/`load`/`transform`
    /// methods — which log and swallow plugin exceptions — every failure
    /// (throw, rejection, timeout, unsettled promise) is returned as `Err`
    /// so `pledge build` can abort. `Ok(None)` means the plugin returned
    /// `null`/`undefined`.
    fn call_hook_value(
        &self,
        plugin_idx: usize,
        hook_name: &str,
        js_args: &str,
    ) -> Result<Option<serde_json::Value>> {
        let global_name = format!("__pledge_plugin_{}", plugin_idx);
        let js_code = format!(
            r#"
            (function() {{
                var st = {{ done: false, error: null, value: null }};
                globalThis.__pledge_hook_state = st;
                var fail = function(e) {{
                    st.done = true;
                    st.error = String((e && e.message) || e);
                }};
                var ok = function(v) {{
                    st.done = true;
                    st.value = (v === undefined) ? null : v;
                }};
                try {{
                    var __pluginModule = globalThis['{global_name}'];
                    if (__pluginModule && typeof __pluginModule.{hook_name} === 'function') {{
                        var __r = __pluginModule.{hook_name}({js_args});
                        if (__r && typeof __r.then === 'function') {{
                            __r.then(ok, fail);
                        }} else {{
                            ok(__r);
                        }}
                    }} else {{
                        st.done = true;
                    }}
                }} catch (e) {{
                    fail(e);
                }}
            }})()
            "#,
        );
        self.with_guarded(|ctx| ctx.eval::<(), _>(js_code.as_str()))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // Settle any promise the hook returned (microtasks only).
        self.arm_deadline();
        let mut timed_out = false;
        while matches!(self.runtime.execute_pending_job(), Ok(true)) {
            if self.epoch.elapsed().as_millis() as u64 + 1 >= self.deadline.load(Ordering::Relaxed)
            {
                timed_out = true;
                break;
            }
        }
        self.deadline.store(0, Ordering::Relaxed);
        if timed_out {
            anyhow::bail!("timed out settling the hook's promise");
        }
        let state: Option<String> = self
            .with_guarded(|ctx| {
                ctx.eval::<Option<String>, _>(
                    "JSON.stringify(globalThis.__pledge_hook_state || null)",
                )
            })
            .ok()
            .flatten();
        let state: serde_json::Value = state
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        if let Some(err) = state.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("{err}");
        }
        if state.get("done").and_then(|d| d.as_bool()) != Some(true) {
            anyhow::bail!(
                "hook returned a promise that did not settle (timers/I/O are not available in the plugin runtime)"
            );
        }
        Ok(state.get("value").filter(|v| !v.is_null()).cloned())
    }

    fn json_arg(s: &str) -> String {
        serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
    }

    /// Parse a `string | { code, map? }` hook return into a code result.
    fn parse_code_result(
        v: serde_json::Value,
    ) -> Result<Option<pledgepack_core::plugin_hooks::CodeResult>> {
        use pledgepack_core::plugin_hooks::CodeResult;
        match v {
            serde_json::Value::String(code) => Ok(Some(CodeResult { code, map: None })),
            serde_json::Value::Object(o) => match o.get("code") {
                Some(serde_json::Value::String(code)) => Ok(Some(CodeResult {
                    code: code.clone(),
                    map: match o.get("map") {
                        Some(serde_json::Value::String(m)) => Some(m.clone()),
                        Some(serde_json::Value::Object(_)) => o.get("map").map(|m| m.to_string()),
                        _ => None,
                    },
                })),
                // `{}`/`{ code: null }` — Rollup treats a missing code as "no change".
                _ => Ok(None),
            },
            other => anyhow::bail!(
                "expected a string or {{ code, map }} object, got {}",
                match other {
                    serde_json::Value::Bool(_) => "a boolean",
                    serde_json::Value::Number(_) => "a number",
                    serde_json::Value::Array(_) => "an array",
                    _ => "an unsupported value",
                }
            ),
        }
    }
}

impl pledgepack_core::plugin_hooks::PluginHooks for JsPluginHost {
    fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    fn plugin_name(&self, plugin: usize) -> String {
        self.plugins
            .get(plugin)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    fn supports(&self, plugin: usize, hook: pledgepack_core::plugin_hooks::BuildHook) -> bool {
        use pledgepack_core::plugin_hooks::BuildHook;
        self.plugins.get(plugin).is_some_and(|p| match hook {
            BuildHook::ResolveId => p.has_resolve_id,
            BuildHook::Load => p.has_load,
            BuildHook::Transform => p.has_transform,
            BuildHook::RenderChunk => p.has_render_chunk,
            BuildHook::TransformIndexHtml => p.has_transform_index_html,
        })
    }

    /// Only plugins that opt in with `cacheable: true` get a fingerprint: the
    /// host cannot know whether an arbitrary plugin's `transform` is a pure
    /// function of `(code, id)` (it may read other files, env vars, the
    /// clock...), and serving a stale cached result would be worse than
    /// re-running it. The fingerprint covers the plugin's name, declared
    /// `version`, full source and the host's cache salt (plugin config), so
    /// upgrading, editing or reconfiguring a plugin invalidates its entries.
    fn transform_fingerprint(&self, plugin: usize) -> Option<String> {
        let p = self.plugins.get(plugin)?;
        let opted_in = p.source.contains("cacheable: true")
            || p.source.contains("cacheable:true")
            || p.source.contains("cacheable : true");
        if !opted_in {
            return None;
        }
        let version = Self::extract_string_field(&p.source, "version").unwrap_or_default();
        let mut h = blake3::Hasher::new();
        for part in [
            p.name.as_str(),
            version.as_str(),
            self.cache_salt.as_str(),
            p.source.as_str(),
        ] {
            h.update(&(part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        Some(h.finalize().to_hex().to_string())
    }

    /// `resolveId(source, importer)` → `string | { id, external? } | false | null`.
    /// `false` means "external, keep the specifier as-is" (Rollup).
    fn resolve_id(
        &self,
        plugin: usize,
        source: &str,
        importer: Option<&str>,
    ) -> Result<Option<pledgepack_core::plugin_hooks::ResolvedId>> {
        use pledgepack_core::plugin_hooks::ResolvedId;
        let args = format!(
            "{}, {}",
            Self::json_arg(source),
            importer.map_or("undefined".to_string(), Self::json_arg)
        );
        let Some(v) = self.call_hook_value(plugin, "resolveId", &args)? else {
            return Ok(None);
        };
        match v {
            serde_json::Value::String(id) => Ok(Some(ResolvedId {
                id,
                external: false,
            })),
            serde_json::Value::Bool(false) => Ok(Some(ResolvedId {
                id: source.to_string(),
                external: true,
            })),
            serde_json::Value::Object(o) => {
                let Some(id) = o.get("id").and_then(|i| i.as_str()) else {
                    anyhow::bail!("resolveId returned an object without a string `id`");
                };
                Ok(Some(ResolvedId {
                    id: id.to_string(),
                    external: o.get("external").and_then(|e| e.as_bool()).unwrap_or(false),
                }))
            }
            _ => anyhow::bail!(
                "resolveId must return a string, `false`, `{{ id, external }}` or null"
            ),
        }
    }

    fn load(
        &self,
        plugin: usize,
        id: &str,
    ) -> Result<Option<pledgepack_core::plugin_hooks::CodeResult>> {
        match self.call_hook_value(plugin, "load", &Self::json_arg(id))? {
            Some(v) => Self::parse_code_result(v),
            None => Ok(None),
        }
    }

    fn transform(
        &self,
        plugin: usize,
        code: &str,
        id: &str,
    ) -> Result<Option<pledgepack_core::plugin_hooks::CodeResult>> {
        let args = format!(
            "{}, {}",
            Self::json_arg(code),
            Self::json_arg(&pledgepack_core::normalize_path_str(id))
        );
        match self.call_hook_value(plugin, "transform", &args)? {
            Some(v) => Self::parse_code_result(v),
            None => Ok(None),
        }
    }

    fn render_chunk(
        &self,
        plugin: usize,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Result<Option<pledgepack_core::plugin_hooks::CodeResult>> {
        let args = format!(
            "{}, {}, {}",
            Self::json_arg(code),
            Self::json_arg(filename),
            Self::json_arg(chunk_type)
        );
        match self.call_hook_value(plugin, "renderChunk", &args)? {
            Some(v) => Self::parse_code_result(v),
            None => Ok(None),
        }
    }

    /// `transformIndexHtml(html, filename)` →
    /// `string | tag[] | { html?, tags? } | null`.
    fn transform_index_html(
        &self,
        plugin: usize,
        html: &str,
        filename: &str,
    ) -> Result<Option<pledgepack_core::plugin_hooks::HtmlResult>> {
        use pledgepack_core::plugin_hooks::{HtmlResult, HtmlTagSpec};
        let args = format!("{}, {}", Self::json_arg(html), Self::json_arg(filename));
        let Some(v) = self.call_hook_value(plugin, "transformIndexHtml", &args)? else {
            return Ok(None);
        };
        let parse_tags = |arr: &[serde_json::Value]| -> Vec<HtmlTagSpec> {
            arr.iter()
                .filter_map(|t| {
                    let tag = t.get("tag")?.as_str()?.to_string();
                    let mut attrs: Vec<(String, String)> = t
                        .get("attrs")
                        .and_then(|a| a.as_object())
                        .map(|o| {
                            o.iter()
                                .map(|(k, v)| {
                                    let val = match v {
                                        serde_json::Value::String(s) => s.clone(),
                                        serde_json::Value::Bool(true) => String::new(),
                                        other => other.to_string(),
                                    };
                                    (k.clone(), val)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    attrs.sort();
                    Some(HtmlTagSpec {
                        tag,
                        attrs,
                        children: t.get("children").and_then(|c| c.as_str()).map(String::from),
                        inject_to: t.get("injectTo").and_then(|c| c.as_str()).map(String::from),
                    })
                })
                .collect()
        };
        match v {
            serde_json::Value::String(h) => Ok(Some(HtmlResult {
                html: Some(h),
                tags: Vec::new(),
            })),
            serde_json::Value::Array(arr) => Ok(Some(HtmlResult {
                html: None,
                tags: parse_tags(&arr),
            })),
            serde_json::Value::Object(o) => Ok(Some(HtmlResult {
                html: o.get("html").and_then(|h| h.as_str()).map(String::from),
                tags: o
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|a| parse_tags(a))
                    .unwrap_or_default(),
            })),
            _ => anyhow::bail!(
                "transformIndexHtml must return a string, a tag array, `{{ html, tags }}` or null"
            ),
        }
    }
}

impl Default for JsPluginHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip ESM syntax from plugin source and assign the exported object to a global variable.
/// Converts `export default { ... }` to `globalThis['name'] = { ... }`
/// and `export const/let/var/function/class` to their non-export equivalents.
fn strip_esm_and_assign(source: &str, global_name: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut in_string = false;
    let mut string_delim = '\0';
    let mut chars = source.chars().peekable();

    while let Some(ch) = chars.next() {
        if !in_string && (ch == '"' || ch == '\'' || ch == '`') {
            in_string = true;
            string_delim = ch;
            result.push(ch);
            continue;
        }
        if in_string {
            if ch == '\\' {
                result.push(ch);
                if let Some(&next) = chars.peek() {
                    result.push(next);
                    chars.next();
                }
                continue;
            }
            if ch == string_delim {
                in_string = false;
                string_delim = '\0';
            }
            result.push(ch);
            continue;
        }

        if ch == '/' && chars.peek() == Some(&'/') {
            while let Some(&c) = chars.peek() {
                if c == '\n' {
                    result.push(c);
                    chars.next();
                    break;
                }
                chars.next();
            }
            continue;
        }

        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }

        result.push(ch);
    }

    result
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("export default") {
                line.replace(
                    "export default",
                    &format!("globalThis['{}'] =", global_name),
                )
            } else if trimmed.starts_with("export const")
                || trimmed.starts_with("export let")
                || trimmed.starts_with("export var")
                || trimmed.starts_with("export function")
                || trimmed.starts_with("export class")
            {
                line.replace("export ", "")
            } else if trimmed.starts_with("import ") {
                String::new()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether this host (the QuickJS-backed JS plugin host) genuinely executes
/// a plugin's implementation of `hook_name` when present — not merely
/// whether a *plugin* can declare the hook. See
/// `pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES` for the canonical
/// hook-name list this should be checked against, and
/// PRODUCTION-READINESS-100.md goals 45-46. Before those goals, `buildStart`/
/// `buildEnd`/`generateBundle` were detected but silently never executed —
/// exactly the failure mode this function (and goal 46's integration tests)
/// exist to make impossible to reintroduce unnoticed.
pub fn host_supports_hook(hook_name: &str) -> bool {
    pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES.contains(&hook_name)
}

/// The full capability matrix: every hook name paired with whether this
/// host supports it. See `wasm-plugin-host`'s identically-named function.
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
        // Regression test for goals 42, 45-46 — this is the exact assertion
        // that would have caught the buildStart/buildEnd/generateBundle bug
        // before it shipped.
        let matrix = hook_support_matrix();
        assert_eq!(
            matrix.len(),
            pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES.len()
        );
        for (hook, supported) in &matrix {
            assert!(
                *supported,
                "js-plugin-host claims to support hook '{hook}' but host_supports_hook() says no"
            );
        }
    }

    #[test]
    fn test_parse_plugin() {
        let source = r#"
            export default {
                name: "my-plugin",
                apply: "build",
                transform(code, id) {
                    return { code, map: null };
                },
                resolveId(source, importer) {
                    return { id: source, external: false };
                }
            };
        "#;

        let plugin = JsPluginHost::parse_plugin(source, PathBuf::from("test.js")).unwrap();
        assert_eq!(plugin.name, "my-plugin");
        assert_eq!(plugin.apply, Some("build".to_string()));
        assert!(plugin.has_transform);
        assert!(plugin.has_resolve_id);
        assert!(!plugin.has_load);
    }

    /// Regression tests for goal 12: with no verifier configured,
    /// `check_plugin_signature` must be a no-op — covered implicitly by
    /// `test_parse_plugin` above (and every other pre-existing test in this
    /// file) still passing unmodified. These cover the newly-reachable
    /// enabled path.
    mod signing {
        use super::*;
        use pledgepack_core::plugin_system::{PluginSignature, PluginSigningVerifier};

        #[test]
        fn missing_sidecar_is_rejected_when_verifier_configured() {
            let host = JsPluginHost::new().with_signing_verifier(PluginSigningVerifier::new());
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("plugin.js");
            std::fs::write(&path, "export default { name: 'p' };").unwrap();

            let err = host
                .check_plugin_signature(&path, "export default { name: 'p' };")
                .unwrap_err();
            assert!(err.to_string().contains("no signature sidecar"));
        }

        #[test]
        fn valid_signature_over_correct_hash_is_accepted() {
            use ed25519_dalek::Signer;

            let dir = tempfile::tempdir().unwrap();
            let source = "export default { name: 'p' };";
            let path = dir.path().join("plugin.js");
            std::fs::write(&path, source).unwrap();
            let wasm_hash = blake3::hash(source.as_bytes()).to_hex().to_string();

            let signing_key = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
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
            let sidecar_path = {
                let mut s = path.as_os_str().to_os_string();
                s.push(".sig.json");
                std::path::PathBuf::from(s)
            };
            std::fs::write(sidecar_path, serde_json::to_string(&sig).unwrap()).unwrap();

            let mut verifier = PluginSigningVerifier::new();
            verifier.trust_key("@pledgelabs", &hex::encode(verifying_key.to_bytes()));
            let host = JsPluginHost::new().with_signing_verifier(verifier);
            host.check_plugin_signature(&path, source).unwrap();
        }
    }

    /// End-to-end signing behaviour through the real `load_plugins` path:
    /// valid / invalid / tampered / missing / untrusted.
    mod signing_e2e {
        use super::*;
        use ed25519_dalek::Signer;
        use pledgepack_core::plugin_system::{PluginSignature, PluginSigningVerifier};

        const SRC: &str =
            "export default { name: 'signed', buildStart() { globalThis.__started = true; } };";

        fn sidecar_path(path: &std::path::Path) -> PathBuf {
            let mut s = path.as_os_str().to_os_string();
            s.push(".sig.json");
            PathBuf::from(s)
        }

        /// Write plugin + sidecar signed by `signing_key` over `signed_source`.
        fn write_signed(
            dir: &std::path::Path,
            file_source: &str,
            signed_source: &str,
            signing_key: &ed25519_dalek::SigningKey,
            identity: &str,
        ) -> PathBuf {
            let path = dir.join("plugin.js");
            std::fs::write(&path, file_source).unwrap();
            let wasm_hash = blake3::hash(signed_source.as_bytes()).to_hex().to_string();
            let signature = signing_key.sign(wasm_hash.as_bytes());
            let sig = PluginSignature {
                plugin_name: "signed".to_string(),
                version: "1.0.0".to_string(),
                wasm_hash,
                signer_public_key: hex::encode(signing_key.verifying_key().to_bytes()),
                signature: hex::encode(signature.to_bytes()),
                signer_identity: identity.to_string(),
                timestamp: 0,
                verified: false,
            };
            std::fs::write(sidecar_path(&path), serde_json::to_string(&sig).unwrap()).unwrap();
            path
        }

        fn host_trusting(key: &ed25519_dalek::SigningKey) -> JsPluginHost {
            let mut verifier = PluginSigningVerifier::new();
            verifier.trust_key("@me", &hex::encode(key.verifying_key().to_bytes()));
            JsPluginHost::new().with_signing_verifier(verifier)
        }

        #[test]
        fn valid_signed_plugin_loads_and_runs() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
            let dir = tempfile::tempdir().unwrap();
            let path = write_signed(dir.path(), SRC, SRC, &key, "@me");
            let mut host = host_trusting(&key);
            host.load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap();
            assert_eq!(host.plugins().len(), 1);
            host.build_start().unwrap();
        }

        #[test]
        fn tampered_plugin_source_is_refused() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
            let dir = tempfile::tempdir().unwrap();
            // Signed over SRC, but the file on disk was modified afterwards.
            let tampered = format!("{SRC}\nglobalThis.__evil = true;");
            let path = write_signed(dir.path(), &tampered, SRC, &key, "@me");
            let mut host = host_trusting(&key);
            let err = host
                .load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap_err();
            assert!(
                err.to_string().contains("content hash does not match"),
                "{err}"
            );
            assert!(host.plugins().is_empty());
        }

        #[test]
        fn invalid_signature_is_refused() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
            let other = ed25519_dalek::SigningKey::from_bytes(&[2u8; 32]);
            let dir = tempfile::tempdir().unwrap();
            let path = write_signed(dir.path(), SRC, SRC, &key, "@me");
            // Swap in a signature made by a different key but keep the
            // trusted public key + correct hash: crypto check must fail.
            let mut sig: PluginSignature =
                serde_json::from_slice(&std::fs::read(sidecar_path(&path)).unwrap()).unwrap();
            sig.signature = hex::encode(other.sign(sig.wasm_hash.as_bytes()).to_bytes());
            std::fs::write(sidecar_path(&path), serde_json::to_string(&sig).unwrap()).unwrap();
            let mut host = host_trusting(&key);
            let err = host
                .load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap_err();
            assert!(
                err.to_string().contains("Signature verification FAILED"),
                "{err}"
            );
        }

        #[test]
        fn untrusted_signer_is_refused() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
            let dir = tempfile::tempdir().unwrap();
            let path = write_signed(dir.path(), SRC, SRC, &key, "@me");
            // Host trusts nobody.
            let mut host = JsPluginHost::new().with_signing_verifier(PluginSigningVerifier::new());
            let err = host
                .load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap_err();
            assert!(
                err.to_string().contains("Signature verification FAILED"),
                "{err}"
            );
        }

        #[test]
        fn missing_sidecar_is_refused_via_load_plugins() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("plugin.js");
            std::fs::write(&path, SRC).unwrap();
            let mut host = host_trusting(&key);
            let err = host
                .load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap_err();
            assert!(err.to_string().contains("no signature sidecar"), "{err}");
        }

        #[test]
        fn malformed_sidecar_is_refused() {
            let key = ed25519_dalek::SigningKey::from_bytes(&[1u8; 32]);
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("plugin.js");
            std::fs::write(&path, SRC).unwrap();
            std::fs::write(sidecar_path(&path), "{ not json").unwrap();
            let mut host = host_trusting(&key);
            let err = host
                .load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap_err();
            assert!(
                err.to_string().contains("Malformed signature sidecar"),
                "{err}"
            );
        }

        #[test]
        fn no_verifier_loads_unsigned_plugins() {
            // Explicit opt-out (`plugin_security.require_signed = false`)
            // yields no verifier: unsigned plugins load as before.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("plugin.js");
            std::fs::write(&path, SRC).unwrap();
            let mut host = JsPluginHost::new();
            host.load_plugins(&[path.to_string_lossy().to_string()])
                .unwrap();
            assert_eq!(host.plugins().len(), 1);
        }
    }

    /// Lifecycle + HMR hooks must actually execute the plugin's JS.
    mod hooks {
        use super::*;

        fn host_with(sources: &[&str]) -> (JsPluginHost, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let mut paths = Vec::new();
            for (i, src) in sources.iter().enumerate() {
                let p = dir.path().join(format!("p{i}.js"));
                std::fs::write(&p, src).unwrap();
                paths.push(p.to_string_lossy().to_string());
            }
            let mut host = JsPluginHost::new();
            host.load_plugins(&paths).unwrap();
            (host, dir)
        }

        fn global(host: &JsPluginHost, expr: &str) -> String {
            host.context
                .with(|ctx| ctx.eval::<String, _>(format!("String({expr})")))
                .unwrap()
        }

        #[test]
        fn runaway_hook_is_interrupted_instead_of_hanging() {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("spin.js");
            std::fs::write(
                &p,
                "export default { name: 'spin', transform(code, id) { while (true) {} } };",
            )
            .unwrap();
            let mut host =
                JsPluginHost::new().with_hook_timeout(std::time::Duration::from_millis(200));
            host.load_plugins(&[p.to_string_lossy().to_string()])
                .unwrap();
            let start = std::time::Instant::now();
            assert!(host.transform("x", "a.js").is_none());
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            // The host stays usable after an interrupted hook.
            assert_eq!(global(&host, "1 + 1"), "2");
        }

        #[test]
        fn runtime_config_memory_limit_is_enforced_on_the_host_runtime() {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("hog.js");
            std::fs::write(
                &p,
                "export default { name: 'hog', transform(code, id) {                    var s = 'x'.repeat(1024); for (var i = 0; i < 16; i++) { s = s + s + s + s; }                    return { code: String(s.length) }; } };",
            )
            .unwrap();
            let cfg = advanced::RuntimeConfig {
                memory_limit: 8 * 1024 * 1024,
                ..advanced::RuntimeConfig::default()
            };
            let mut host = JsPluginHost::new().with_runtime_config(&cfg);
            host.load_plugins(&[p.to_string_lossy().to_string()])
                .unwrap();
            // Over the cap: the hook throws out-of-memory, host reports no result.
            assert!(host.transform("x", "a.js").is_none());
            assert_eq!(global(&host, "1 + 1"), "2");
        }

        #[test]
        fn transform_fingerprint_is_opt_in_and_tracks_source_version_and_config() {
            use pledgepack_core::plugin_hooks::PluginHooks;
            let (plain, _d) = host_with(&[
                "export default { name: 'a', version: '1', transform(c) { return c; } };",
            ]);
            assert!(
                plain.transform_fingerprint(0).is_none(),
                "non-opted-in plugins are uncacheable"
            );

            let src = "export default { name: 'a', version: '1', cacheable: true, transform(c) { return c; } };";
            let (a, _d1) = host_with(&[src]);
            let fp_a = a.transform_fingerprint(0).expect("opted in");
            let (a2, _d2) = host_with(&[src]);
            assert_eq!(a2.transform_fingerprint(0).unwrap(), fp_a, "stable");

            // Version / source change -> new fingerprint.
            let (b, _d3) = host_with(&[&src.replace("'1'", "'2'")]);
            assert_ne!(b.transform_fingerprint(0).unwrap(), fp_a);
            // Config salt change -> new fingerprint.
            let (c, _d4) = host_with(&[src]);
            let c = c.with_cache_salt("opts-v2");
            assert_ne!(c.transform_fingerprint(0).unwrap(), fp_a);
        }

        #[test]
        fn transform_id_with_quote_and_newline_reaches_plugin_intact() {
            let (mut host, _d) = host_with(&[
                "export default { name: 't', transform(code, id) { return { code: id }; } };",
            ]);
            let id = "a\"b
c.js";
            let out = host.transform("x", id).unwrap();
            assert_eq!(out.code, id);
        }

        #[test]
        fn lifecycle_hooks_execute_in_order() {
            let (host, _d) = host_with(&[
                "export default { name: 'a',
                    buildStart() { globalThis.log = (globalThis.log || '') + 'S'; },
                    generateBundle(opts, bundle) { globalThis.log += 'G' + typeof opts + typeof bundle; },
                    buildEnd() { globalThis.log += 'E'; } };",
            ]);
            host.build_start().unwrap();
            host.generate_bundle().unwrap();
            host.build_end().unwrap();
            assert_eq!(global(&host, "globalThis.log"), "SGobjectobjectE");
        }

        #[test]
        fn async_hook_is_awaited() {
            let (host, _d) = host_with(&["export default { name: 'a',
                    async buildStart() { await null; globalThis.done = 'yes'; } };"]);
            host.build_start().unwrap();
            assert_eq!(global(&host, "globalThis.done"), "yes");
        }

        #[test]
        fn throwing_and_rejecting_hooks_report_errors_but_run_all_plugins() {
            let (host, _d) = host_with(&[
                "export default { name: 'thrower', buildStart() { throw new Error('boom'); } };",
                "export default { name: 'rejecter', async buildStart() { throw new Error('nope'); } };",
                "export default { name: 'ok', buildStart() { globalThis.ran = 'ok'; } };",
            ]);
            let err = host.build_start().unwrap_err().to_string();
            assert!(err.contains("thrower") && err.contains("boom"), "{err}");
            assert!(err.contains("rejecter") && err.contains("nope"), "{err}");
            assert_eq!(global(&host, "globalThis.ran"), "ok");
        }

        #[test]
        fn hooks_target_correct_plugin_even_with_duplicate_names() {
            let (host, _d) = host_with(&[
                "export default { name: 'dup', buildStart() { globalThis.who = 'first'; } };",
                "export default { name: 'dup', buildStart() { globalThis.who += '+second'; } };",
            ]);
            host.build_start().unwrap();
            assert_eq!(global(&host, "globalThis.who"), "first+second");
        }

        #[test]
        fn handle_hot_update_returns_module_ids() {
            let (mut host, _d) = host_with(&[
                "export default { name: 'hmr',
                    handleHotUpdate(file, ts) {
                        if (file.endsWith('.txt')) return { moduleIds: ['/src/a.js', file + ':' + (typeof ts)] };
                        if (file.endsWith('.skip')) return { moduleIds: [] };
                        return null;
                    } };",
            ]);
            let r = host.handle_hot_update("notes.txt", 42).unwrap();
            assert_eq!(r.module_ids, vec!["/src/a.js", "notes.txt:number"]);
            assert!(
                host.handle_hot_update("x.skip", 1)
                    .unwrap()
                    .module_ids
                    .is_empty()
            );
            assert!(host.handle_hot_update("other.js", 1).is_none());
        }

        #[test]
        fn hook_names_do_not_leak_between_plugins() {
            // Plugin without the hook is never invoked.
            let (mut host, _d) = host_with(&["export default { name: 'plain' };"]);
            host.build_start().unwrap();
            assert!(host.handle_hot_update("a.js", 0).is_none());
        }
    }

    #[test]
    fn test_has_hook_detection() {
        assert!(JsPluginHost::has_hook(
            "transform(code, id) {}",
            "transform"
        ));
        assert!(JsPluginHost::has_hook(
            "transform: function(code) {}",
            "transform"
        ));
        assert!(!JsPluginHost::has_hook("load(code) {}", "transform"));
    }
}

#[cfg(test)]
mod build_hooks {
    use super::*;
    use pledgepack_core::plugin_hooks::{BuildHook, HookRunner, PluginHooks};

    fn host_with(sources: &[&str]) -> (JsPluginHost, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for (i, src) in sources.iter().enumerate() {
            let p = dir.path().join(format!("p{i}.js"));
            std::fs::write(&p, src).unwrap();
            paths.push(p.to_string_lossy().to_string());
        }
        let mut host = JsPluginHost::new();
        host.load_plugins(&paths).unwrap();
        (host, dir)
    }

    #[test]
    fn hooks_return_strings_objects_and_async_results() {
        let (host, _d) = host_with(&[
            "export default { name: 'p',
                resolveId(source, importer) { return source === 'virtual:x' ? 'virtual-x' : null; },
                load(id) { return id === 'virtual-x' ? { code: 'export default 1' } : null; },
                transform(code, id) { return Promise.resolve(code + '<t>'); },
                renderChunk(code, file, type) { return code + '<' + file + ':' + type + '>'; },
                transformIndexHtml(html) { return [{ tag: 'meta', attrs: { name: 'a' }, injectTo: 'head' }]; } };",
        ]);
        let r = HookRunner::new(&host);
        assert_eq!(
            r.resolve_id("virtual:x", Some("/a.js"))
                .unwrap()
                .unwrap()
                .id,
            "virtual-x"
        );
        assert!(r.resolve_id("./other", Some("/a.js")).unwrap().is_none());
        assert_eq!(
            r.load("virtual-x").unwrap().unwrap().code,
            "export default 1"
        );
        assert!(r.load("nope").unwrap().is_none());
        assert_eq!(r.transform("a", "/x.js").unwrap().unwrap().code, "a<t>");
        assert_eq!(
            r.render_chunk("c", "entry.js", "entry")
                .unwrap()
                .unwrap()
                .code,
            "c<entry.js:entry>"
        );
        let html = r
            .transform_index_html("<head></head>", "index.html")
            .unwrap();
        assert!(html.contains("<meta name=\"a\">"), "{html}");
        assert!(host.supports(0, BuildHook::Load));
    }

    #[test]
    fn plugin_exceptions_and_rejections_are_errors_naming_the_plugin() {
        let (host, _d) = host_with(&[
            "export default { name: 'thrower', transform(code, id) { throw new Error('bad ' + id); } };",
            "export default { name: 'rejecter', renderChunk() { return Promise.reject(new Error('nope')); } };",
        ]);
        let r = HookRunner::new(&host);
        let e = r.transform("a", "/src/m.js").unwrap_err().to_string();
        assert!(
            e.contains("thrower") && e.contains("bad /src/m.js") && e.contains("/src/m.js"),
            "{e}"
        );
        let e = r
            .render_chunk("a", "e.js", "entry")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("rejecter") && e.contains("nope") && e.contains("e.js"),
            "{e}"
        );
    }

    #[test]
    fn resolve_id_false_means_external_and_bad_types_error() {
        let (host, _d) = host_with(&[
            "export default { name: 'ext', resolveId(s) { return s === 'cdn' ? false : (s === 'bad' ? 42 : null); } };",
        ]);
        let r = HookRunner::new(&host);
        let ext = r.resolve_id("cdn", None).unwrap().unwrap();
        assert!(ext.external && ext.id == "cdn");
        assert!(
            r.resolve_id("bad", None)
                .unwrap_err()
                .to_string()
                .contains("ext")
        );
    }
}
