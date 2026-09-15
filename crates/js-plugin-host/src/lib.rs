// JS Plugin Host — Vite-compatible plugin API
//
// Provides a JavaScript plugin interface that mirrors Vite's plugin hooks:
//   - resolveId(source, importer) → { id, external } | null
//   - load(id) → { code, map } | null

pub mod advanced;
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
use tracing::{info, warn};

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
}

impl JsPluginHost {
    /// Create a new empty plugin host with a JS runtime
    pub fn new() -> Self {
        let runtime = Runtime::new().expect("Failed to create QuickJS runtime");
        let context = Context::full(&runtime).expect("Failed to create QuickJS context");

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
        }
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

    /// Verify `path` against a `<path>.sig.json` sidecar when a signing
    /// verifier is configured. No-op (matching prior behavior exactly) when
    /// none is. See `pledgepack_wasm_plugin_host`'s identically-named check
    /// for the full rationale.
    fn check_plugin_signature(&self, path: &std::path::Path, source: &str) -> Result<()> {
        let Some(ref verifier) = self.signing_verifier else {
            return Ok(());
        };

        let sidecar_path = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".sig.json");
            PathBuf::from(s)
        };
        let sidecar_bytes = std::fs::read(&sidecar_path).map_err(|_| {
            anyhow::anyhow!(
                "Plugin {} has no signature sidecar ({}), but signing enforcement is enabled — refusing to load",
                path.display(),
                sidecar_path.display()
            )
        })?;
        let sig: pledgepack_core::plugin_system::PluginSignature =
            serde_json::from_slice(&sidecar_bytes).map_err(|e| {
                anyhow::anyhow!(
                    "Malformed signature sidecar {}: {e} — refusing to load {}",
                    sidecar_path.display(),
                    path.display()
                )
            })?;

        let actual_hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        if actual_hash != sig.wasm_hash {
            anyhow::bail!(
                "Plugin {} content hash does not match its signature sidecar — refusing to load (expected {}, got {})",
                path.display(),
                sig.wasm_hash,
                actual_hash
            );
        }
        if !verifier.verify(&sig) {
            anyhow::bail!(
                "Signature verification FAILED for plugin {} — refusing to load",
                path.display()
            );
        }
        info!("Plugin {}: signature verified ({})", path.display(), sig.signer_identity);
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
            if let Err(e) = self
                .context
                .with(|ctx| ctx.eval::<(), _>(js_source.as_str()))
            {
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
        if !dir.is_dir() {
            return Ok(host);
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
            host.load_plugins(&plugin_paths)?;
        }
        Ok(host)
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

    /// Run a no-argument, no-return lifecycle hook (`buildStart`/`buildEnd`/
    /// `generateBundle`) across all plugins that declare it, by name.
    ///
    /// Previously `build_start`/`build_end`/`generate_bundle` only logged
    /// that a plugin *had* the hook without ever calling the plugin's JS
    /// function — unlike `resolve_id`/`load`/`transform` above, which
    /// genuinely `eval` into the plugin's QuickJS context. This shares that
    /// same eval pattern instead of duplicating it three times. See
    /// PRODUCTION-READINESS-100.md goal 42.
    fn run_lifecycle_hook(&self, hook_name: &str, has_hook: impl Fn(&JsPlugin) -> bool) {
        for (index, plugin) in self.plugins.iter().enumerate() {
            if !has_hook(plugin) {
                continue;
            }
            info!("[plugin:{}] {}", plugin.name, hook_name);
            let global_name = format!("__pledge_plugin_{}", index);
            let js_code = format!(
                r#"
                (function() {{
                    try {{
                        var __pluginModule = globalThis['{global_name}'];
                        if (__pluginModule && typeof __pluginModule.{hook_name} === 'function') {{
                            __pluginModule.{hook_name}();
                        }}
                    }} catch(e) {{
                        console.log('Plugin {hook_name} error: ' + e.message);
                    }}
                }})()
                "#,
            );
            if let Err(e) = self.context.with(|ctx| ctx.eval::<(), _>(js_code.as_str())) {
                warn!("[plugin:{}] {} execution error: {}", plugin.name, hook_name, e);
            }
        }
    }

    /// Run buildStart hooks for all plugins.
    pub fn build_start(&self) {
        self.run_lifecycle_hook("buildStart", |p| p.has_build_start);
    }

    /// Run buildEnd hooks for all plugins.
    pub fn build_end(&self) {
        self.run_lifecycle_hook("buildEnd", |p| p.has_build_end);
    }

    /// Run generateBundle hooks for all plugins.
    pub fn generate_bundle(&self) {
        self.run_lifecycle_hook("generateBundle", |p| p.has_generate_bundle);
    }

    /// Check if any plugin handles resolveId for the given source
    /// Actually calls the JS resolveId() function in each plugin that has it
    pub fn resolve_id(&mut self, source: &str, importer: &str) -> Option<ResolveIdResult> {
        for plugin in &self.plugins {
            if plugin.has_resolve_id {
                info!("[plugin:{}] resolveId: {}", plugin.name, source);

                let global_name = format!(
                    "__pledge_plugin_{}",
                    self.plugins
                        .iter()
                        .position(|p| p.name == plugin.name)
                        .unwrap_or(0)
                );
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

                match self
                    .context
                    .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
                {
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
        for plugin in &self.plugins {
            if plugin.has_load {
                info!("[plugin:{}] load: {}", plugin.name, id);

                let global_name = format!(
                    "__pledge_plugin_{}",
                    self.plugins
                        .iter()
                        .position(|p| p.name == plugin.name)
                        .unwrap_or(0)
                );
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

                match self
                    .context
                    .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
                {
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

        for plugin in &self.plugins {
            if plugin.has_transform {
                info!("[plugin:{}] transform: {}", plugin.name, id);

                // Try to call the plugin's transform function in JS
                let global_name = format!(
                    "__pledge_plugin_{}",
                    self.plugins
                        .iter()
                        .position(|p| p.name == plugin.name)
                        .unwrap_or(0)
                );
                let js_code = format!(
                    r#"
                    (function() {{
                        try {{
                            var __pluginModule = globalThis['{}'];
                            if (__pluginModule && typeof __pluginModule.transform === 'function') {{
                                var __result = __pluginModule.transform({}, "{}");
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
                    pledgepack_core::normalize_path_str(id).replace('"', "\\\"")
                );

                match self
                    .context
                    .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
                {
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

        for plugin in &self.plugins {
            if !plugin.has_render_chunk {
                continue;
            }
            info!("[plugin:{}] renderChunk: {}", plugin.name, filename);

            let global_name = format!(
                "__pledge_plugin_{}",
                self.plugins
                    .iter()
                    .position(|p| p.name == plugin.name)
                    .unwrap_or(0)
            );
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

            match self
                .context
                .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
            {
                Ok(Some(json_str)) => {
                    if let Ok(result) = serde_json::from_str::<TransformResult>(&json_str) {
                        result_code = result.code;
                        rendered = true;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("[plugin:{}] renderChunk execution error: {}", plugin.name, e);
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
        for plugin in &self.plugins {
            if !plugin.has_handle_hot_update {
                continue;
            }
            info!("[plugin:{}] handleHotUpdate: {}", plugin.name, file);

            let global_name = format!(
                "__pledge_plugin_{}",
                self.plugins
                    .iter()
                    .position(|p| p.name == plugin.name)
                    .unwrap_or(0)
            );
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

            match self
                .context
                .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
            {
                Ok(Some(json_str)) => {
                    if let Ok(result) = serde_json::from_str::<HotUpdateResult>(&json_str) {
                        return Some(result);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("[plugin:{}] handleHotUpdate execution error: {}", plugin.name, e);
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

        for plugin in &self.plugins {
            if plugin.has_transform_index_html {
                info!("[plugin:{}] transformIndexHtml", plugin.name);

                let global_name = format!(
                    "__pledge_plugin_{}",
                    self.plugins
                        .iter()
                        .position(|p| p.name == plugin.name)
                        .unwrap_or(0)
                );
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

                match self
                    .context
                    .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
                {
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

        for plugin in &self.plugins {
            if plugin.has_configure_server {
                info!("[plugin:{}] configureServer", plugin.name);

                // Execute the configureServer hook in JS
                // The plugin can register middleware by calling server.use(fn)
                let global_name = format!(
                    "__pledge_plugin_{}",
                    self.plugins
                        .iter()
                        .position(|p| p.name == plugin.name)
                        .unwrap_or(0)
                );
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

                match self
                    .context
                    .with(|ctx| ctx.eval::<Option<String>, _>(js_code.as_str()))
                {
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
        assert_eq!(matrix.len(), pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES.len());
        for (hook, supported) in &matrix {
            assert!(*supported, "js-plugin-host claims to support hook '{hook}' but host_supports_hook() says no");
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
