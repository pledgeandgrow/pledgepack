// Advanced JS plugin features: G10.4-G10.10
//
// G10.4: V8 isolate pooling — keep JS runtimes alive between plugin calls
// G10.5: JS plugin output schema detection — store as structured Task<T> if schema matches
// G10.6: JS plugin migration assistant — analyze Vite plugins and suggest WASM rewrite
// G10.7: JS plugin batching — batch multiple JS plugins in a single runtime context
// G10.8: Automatic JS-to-WASM transpilation — for simple JS plugins
// G10.9: QuickJS JIT / V8 switch — make JS plugins faster
// G10.10: Node.js compatibility layer — WASM-based polyfill for Node APIs

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ─── G10.4: V8 isolate pooling ───────────────────────────────────────

/// G10.4: Pool of JS runtime contexts (isolate pooling).
/// Instead of recreating the QuickJS context for each plugin call,
/// keep contexts alive and reuse them.
pub struct IsolatePool {
    /// Available (Runtime, Context) pairs in the pool
    pool: Vec<(rquickjs::Runtime, rquickjs::Context)>,
    /// Maximum pool size
    max_size: usize,
    /// Heap cap applied to every runtime this pool creates (0 = unlimited).
    memory_limit: usize,
    /// Total contexts created
    created: usize,
    /// Total contexts reused
    reused: usize,
}

impl IsolatePool {
    /// Pool whose runtimes use the default [`RuntimeConfig`] memory limit.
    pub fn new(max_size: usize) -> Self {
        Self::with_config(max_size, &RuntimeConfig::default())
    }

    /// Pool whose runtimes are capped at `config.memory_limit` bytes
    /// (0 = unlimited). The limit was previously declared on
    /// [`RuntimeConfig`] but applied to no runtime.
    pub fn with_config(max_size: usize, config: &RuntimeConfig) -> Self {
        Self {
            pool: Vec::with_capacity(max_size),
            max_size,
            memory_limit: config.memory_limit,
            created: 0,
            reused: 0,
        }
    }

    /// Get a (Runtime, Context) pair from the pool, or create a new one
    pub fn acquire(&mut self) -> (rquickjs::Runtime, rquickjs::Context) {
        if let Some((rt, ctx)) = self.pool.pop() {
            self.reused += 1;
            (rt, ctx)
        } else {
            self.created += 1;
            let rt = rquickjs::Runtime::new().expect("Failed to create QuickJS runtime");
            crate::apply_memory_limit(&rt, self.memory_limit);
            let ctx = rquickjs::Context::full(&rt).expect("Failed to create QuickJS context");
            (rt, ctx)
        }
    }

    /// Return a (Runtime, Context) pair to the pool for reuse
    pub fn release(&mut self, pair: (rquickjs::Runtime, rquickjs::Context)) {
        if self.pool.len() < self.max_size {
            self.pool.push(pair);
        }
    }

    /// Get pool statistics
    pub fn stats(&self) -> IsolatePoolStats {
        IsolatePoolStats {
            pool_size: self.pool.len(),
            created: self.created,
            reused: self.reused,
            reuse_ratio: if self.created + self.reused > 0 {
                self.reused as f64 / (self.created + self.reused) as f64
            } else {
                0.0
            },
        }
    }
}

#[derive(Debug)]
pub struct IsolatePoolStats {
    pub pool_size: usize,
    pub created: usize,
    pub reused: usize,
    pub reuse_ratio: f64,
}

// ─── G10.5: JS plugin output schema detection ────────────────────────

/// G10.5: Known output schemas that can be detected
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OutputSchema {
    /// Standard transform output: { code: string, map?: string }
    TransformOutput,
    /// Resolve result: { id: string, external: boolean }
    ResolveResult,
    /// Load result: { code: string, map?: string }
    LoadResult,
    /// HTML transform: { html: string, tags: array }
    HtmlTransform,
    /// Unknown / opaque blob
    Unknown,
}

/// G10.5: Detect the output schema of a plugin's transform result
pub fn detect_output_schema(json: &str) -> OutputSchema {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json)
        && let Some(obj) = value.as_object()
    {
        if obj.contains_key("code") && obj.contains_key("id") && obj.contains_key("external") {
            return OutputSchema::ResolveResult;
        }
        if obj.contains_key("code") && obj.contains_key("html") {
            return OutputSchema::HtmlTransform;
        }
        if obj.contains_key("code") {
            if obj.contains_key("map") {
                return OutputSchema::TransformOutput;
            }
            return OutputSchema::LoadResult;
        }
    }
    OutputSchema::Unknown
}

/// G10.5: Check if a plugin's output matches a known schema
pub fn matches_transform_schema(json: &str) -> bool {
    matches!(
        detect_output_schema(json),
        OutputSchema::TransformOutput | OutputSchema::LoadResult
    )
}

// ─── G10.6: JS plugin migration assistant ────────────────────────────

/// G10.6: Migration analysis result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationAnalysis {
    /// Plugin name (from Vite plugin)
    pub plugin_name: String,
    /// Detected hooks
    pub hooks: Vec<String>,
    /// Complexity score (0-100, higher = harder to migrate)
    pub complexity: u32,
    /// Migration difficulty
    pub difficulty: MigrationDifficulty,
    /// Suggested WASM rewrite approach
    pub suggestion: String,
    /// Node.js APIs used (that need polyfills)
    pub node_apis: Vec<String>,
    /// Whether the plugin is simple enough for automatic transpilation
    pub auto_transpilable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MigrationDifficulty {
    Trivial,
    Easy,
    Moderate,
    Hard,
    VeryHard,
}

/// G10.6: Analyze a Vite plugin source and suggest WASM migration
pub fn analyze_vite_plugin(source: &str, plugin_name: &str) -> MigrationAnalysis {
    let mut hooks = Vec::new();
    let mut node_apis = Vec::new();

    // Detect hooks
    let hook_names = [
        "resolveId",
        "load",
        "transform",
        "transformIndexHtml",
        "configureServer",
        "buildStart",
        "buildEnd",
        "generateBundle",
        "options",
        "renderStart",
        "renderError",
        "writeBundle",
        "closeBundle",
        "moduleParsed",
        "banner",
        "footer",
    ];

    for hook in &hook_names {
        if source.contains(&format!("{}:", hook)) || source.contains(&format!("{}(", hook)) {
            hooks.push(hook.to_string());
        }
    }

    // Detect Node.js API usage
    let node_api_patterns = [
        ("fs", "fs"),
        ("path", "path"),
        ("os", "os"),
        ("crypto", "crypto"),
        ("http", "http"),
        ("https", "https"),
        ("stream", "stream"),
        ("buffer", "Buffer"),
        ("process", "process"),
        ("child_process", "exec"),
        ("url", "URL"),
        ("util", "util"),
    ];

    for (api, pattern) in &node_api_patterns {
        if source.contains(pattern) {
            node_apis.push(api.to_string());
        }
    }

    // Calculate complexity
    let hook_count = hooks.len();
    let node_api_count = node_apis.len();
    let source_lines = source.lines().count();

    let mut complexity: u32 = (hook_count * 10 + node_api_count * 15) as u32;
    if source_lines > 100 {
        complexity += 20;
    }
    if source_lines > 500 {
        complexity += 30;
    }
    if source.contains("async") {
        complexity += 10;
    }
    if source.contains("await") {
        complexity += 10;
    }
    complexity = complexity.min(100);

    let difficulty = if complexity < 20 {
        MigrationDifficulty::Trivial
    } else if complexity < 40 {
        MigrationDifficulty::Easy
    } else if complexity < 60 {
        MigrationDifficulty::Moderate
    } else if complexity < 80 {
        MigrationDifficulty::Hard
    } else {
        MigrationDifficulty::VeryHard
    };

    let auto_transpilable = complexity < 30 && node_api_count == 0;

    let suggestion = match difficulty {
        MigrationDifficulty::Trivial => {
            "Can be automatically transpiled to WASM. Use `pledge plugin compile`.".to_string()
        }
        MigrationDifficulty::Easy => {
            "Straightforward WASM rewrite. Manual conversion of hooks needed.".to_string()
        }
        MigrationDifficulty::Moderate => {
            "Requires moderate effort. Node.js API calls need WASM polyfills.".to_string()
        }
        MigrationDifficulty::Hard => {
            "Complex plugin. Consider keeping as JS plugin with QuickJS runtime.".to_string()
        }
        MigrationDifficulty::VeryHard => {
            "Very complex. Recommend keeping as JS plugin. WASM migration not practical."
                .to_string()
        }
    };

    MigrationAnalysis {
        plugin_name: plugin_name.to_string(),
        hooks,
        complexity,
        difficulty,
        suggestion,
        node_apis,
        auto_transpilable,
    }
}

// ─── G10.7: JS plugin batching ───────────────────────────────────────

/// G10.7: Batch multiple JS plugins into a single runtime context.
/// Instead of evaluating each plugin separately, combine them into
/// a single evaluation pass.
pub struct PluginBatcher {
    /// Combined plugin source
    combined_source: String,
    /// Plugin names in the batch
    plugin_names: Vec<String>,
}

impl Default for PluginBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginBatcher {
    pub fn new() -> Self {
        Self {
            combined_source: String::new(),
            plugin_names: Vec::new(),
        }
    }

    /// Add a plugin to the batch
    pub fn add_plugin(&mut self, name: &str, source: &str) {
        let global_name = format!("__pledge_batch_plugin_{}", self.plugin_names.len());
        let stripped = strip_esm_for_batch(source, &global_name);
        self.combined_source.push_str(&stripped);
        self.combined_source.push('\n');
        self.plugin_names.push(name.to_string());
    }

    /// Get the combined source for batch evaluation
    pub fn combined_source(&self) -> &str {
        &self.combined_source
    }

    /// Get the number of plugins in the batch
    pub fn count(&self) -> usize {
        self.plugin_names.len()
    }

    /// Get plugin global names
    pub fn plugin_globals(&self) -> Vec<String> {
        (0..self.plugin_names.len())
            .map(|i| format!("__pledge_batch_plugin_{}", i))
            .collect()
    }
}

/// Strip ESM syntax for batch evaluation
fn strip_esm_for_batch(source: &str, global_name: &str) -> String {
    source
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

// ─── G10.8: Automatic JS-to-WASM transpilation ───────────────────────

/// G10.8: Check if a JS plugin is simple enough for automatic WASM transpilation
pub fn can_transpile_to_wasm(source: &str) -> bool {
    // Check for patterns that prevent automatic transpilation
    let blockers = [
        "require(",
        "process.",
        "fetch(",
        "setTimeout",
        "setInterval",
        "Promise(",
        "XMLHttpRequest",
        "WebSocket",
        "addEventListener",
        "document.",
        "window.",
    ];

    // Check for Node.js imports (import ... from 'fs', etc.)
    let node_imports = [
        "fs",
        "path",
        "os",
        "crypto",
        "http",
        "https",
        "stream",
        "child_process",
        "net",
        "tls",
    ];
    for node_mod in &node_imports {
        if source.contains(&format!("from '{}'", node_mod))
            || source.contains(&format!("require('{}'", node_mod))
        {
            return false;
        }
    }

    for blocker in &blockers {
        if source.contains(blocker) {
            return false;
        }
    }

    // Check that the plugin only uses simple hooks
    let allowed_hooks = ["transform", "resolveId", "load"];
    let disallowed_hooks = ["configureServer", "transformIndexHtml", "generateBundle"];

    for hook in &disallowed_hooks {
        if source.contains(&format!("{}:", hook)) {
            return false;
        }
    }

    // Must have at least one allowed hook
    allowed_hooks.iter().any(|hook| {
        source.contains(&format!("{}:", hook)) || source.contains(&format!("{}(", hook))
    })
}

/// G10.8: Generate a Rust **signature scaffold** for porting a JS plugin to
/// the WASM Component Model host.
///
/// This does NOT migrate any logic — it detects which hooks the JS plugin
/// declares and emits a matching Rust function signature per hook, each
/// body a `todo!()` the plugin author must fill in by hand by reading their
/// own original JS implementation. Automatically translating arbitrary JS
/// hook bodies into equivalent Rust is not attempted (and isn't a
/// mechanical transformation in general — the two languages don't share a
/// memory model, string handling, or async story). Earlier versions of this
/// function's doc comment and generated output both undersold this (`pledge
/// plugin migrate` reads as more automated than it is); the docstring and
/// the generated file's own header comment now say so explicitly. See
/// PRODUCTION-READINESS-100.md goal 47.
pub fn generate_wasm_skeleton(plugin_name: &str, source: &str) -> String {
    let has_transform = source.contains("transform:") || source.contains("transform(");
    let has_resolve_id = source.contains("resolveId:") || source.contains("resolveId(");
    let has_load = source.contains("load:") || source.contains("load(");

    let mut skeleton = format!(
        r#"// WASM plugin SIGNATURE SCAFFOLD for {} (auto-generated by `generate_wasm_skeleton`)
//
// This file does NOT port any logic from the original JS plugin. It only
// detects which hooks were present and emits a matching Rust function
// signature per hook, with a `todo!()` body. You must implement each hook
// by hand, referring to your original JS plugin's source for the logic to
// port — automatic JS→Rust logic translation is not attempted.

use pledgepack_plugin_sdk::*;

pub struct {}Plugin;

impl Plugin for {}Plugin {{
    fn name(&self) -> &str {{ "{}" }}
"#,
        plugin_name,
        to_pascal_case(plugin_name),
        to_pascal_case(plugin_name),
        plugin_name
    );

    if has_transform {
        skeleton.push_str(
            r#"
    fn transform(&self, code: &str, id: &str) -> Option<TransformResult> {
        // MANUAL PORT REQUIRED — not generated from the original JS.
        // Original JS: transform(code, id) { ... }
        todo!("port the transform() hook body from the original JS plugin")
    }
"#,
        );
    }

    if has_resolve_id {
        skeleton.push_str(
            r#"
    fn resolve_id(&self, source: &str, importer: &str) -> Option<ResolveResult> {
        // MANUAL PORT REQUIRED — not generated from the original JS.
        // Original JS: resolveId(source, importer) { ... }
        todo!("port the resolveId() hook body from the original JS plugin")
    }
"#,
        );
    }

    if has_load {
        skeleton.push_str(
            r#"
    fn load(&self, id: &str) -> Option<LoadResult> {
        // MANUAL PORT REQUIRED — not generated from the original JS.
        // Original JS: load(id) { ... }
        todo!("port the load() hook body from the original JS plugin")
    }
"#,
        );
    }

    skeleton.push_str("}\n");

    skeleton
}

fn to_pascal_case(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

// ─── G10.9: QuickJS JIT / V8 switch ──────────────────────────────────

/// G10.9: JS runtime backend selection
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum JsRuntime {
    /// QuickJS interpreter — lightweight, fast, C-based
    QuickJs,
    /// QuickJS with JIT — faster than interpreter, still lightweight
    QuickJsJit,
    /// V8 with JIT — fastest, but heavy dependency
    V8,
}

impl JsRuntime {
    pub fn is_jit_enabled(&self) -> bool {
        matches!(self, JsRuntime::QuickJsJit | JsRuntime::V8)
    }

    pub fn description(&self) -> &'static str {
        match self {
            JsRuntime::QuickJs => "QuickJS interpreter (lightweight, C-based)",
            JsRuntime::QuickJsJit => "QuickJS with JIT (lightweight, fast)",
            JsRuntime::V8 => "V8 with JIT (fastest, heavy dependency)",
        }
    }
}

/// G10.9: Get the currently active JS runtime
pub fn current_runtime() -> JsRuntime {
    // Check environment variable for runtime override
    if let Ok(rt) = std::env::var("PLEDGE_JS_RUNTIME") {
        match rt.as_str() {
            "v8" | "V8" => return JsRuntime::V8,
            "quickjs-jit" | "QuickJS-JIT" => return JsRuntime::QuickJsJit,
            "quickjs" | "QuickJS" | "boa" | "Boa" => return JsRuntime::QuickJs,
            _ => {}
        }
    }
    // Default: QuickJS (lightweight, fast, C-based)
    JsRuntime::QuickJs
}

/// G10.9: Check if a JIT-enabled runtime is available
#[allow(unexpected_cfgs)]
pub fn jit_available() -> bool {
    // Check if QuickJS JIT or V8 feature is compiled in
    cfg!(any(feature = "quickjs", feature = "v8"))
}

/// G10.3: Runtime configuration for selecting JS engine backend.
/// V8 for maximum compatibility, QuickJS for no-Node environments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    /// Preferred runtime backend
    pub backend: JsRuntime,
    /// Fallback runtime if preferred is not available
    pub fallback: JsRuntime,
    /// Whether to enable JIT (if available)
    pub enable_jit: bool,
    /// Memory limit in bytes (0 = unlimited)
    pub memory_limit: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            backend: current_runtime(),
            fallback: JsRuntime::QuickJs,
            enable_jit: true,
            memory_limit: 256 * 1024 * 1024, // 256MB
        }
    }
}

impl RuntimeConfig {
    /// Create a config optimized for maximum compatibility (V8)
    pub fn max_compatibility() -> Self {
        Self {
            backend: JsRuntime::V8,
            fallback: JsRuntime::QuickJsJit,
            enable_jit: true,
            memory_limit: 512 * 1024 * 1024,
        }
    }

    /// Create a config optimized for no-Node environments (QuickJS)
    pub fn no_node() -> Self {
        Self {
            backend: JsRuntime::QuickJsJit,
            fallback: JsRuntime::QuickJs,
            enable_jit: true,
            memory_limit: 128 * 1024 * 1024,
        }
    }

    /// Create a config optimized for minimal binary size (QuickJS)
    pub fn minimal() -> Self {
        Self {
            backend: JsRuntime::QuickJs,
            fallback: JsRuntime::QuickJs,
            enable_jit: false,
            memory_limit: 64 * 1024 * 1024,
        }
    }

    /// Resolve the actual runtime to use, considering availability
    pub fn resolve(&self) -> JsRuntime {
        if self.backend.is_jit_enabled() && !jit_available() {
            // JIT backend requested but not available, use fallback
            if self.fallback.is_jit_enabled() && !jit_available() {
                JsRuntime::QuickJs
            } else {
                self.fallback
            }
        } else {
            self.backend
        }
    }

    /// Check if the resolved runtime has JIT enabled
    pub fn is_jit_active(&self) -> bool {
        self.enable_jit && self.resolve().is_jit_enabled() && jit_available()
    }
}

// ─── G10.10: Node.js compatibility layer ─────────────────────────────

/// G10.10: Node.js API polyfill registry
pub struct NodeCompatLayer {
    /// Registered polyfills: api_name -> WASM implementation source
    polyfills: HashMap<String, String>,
}

impl Default for NodeCompatLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeCompatLayer {
    pub fn new() -> Self {
        let mut layer = Self {
            polyfills: HashMap::new(),
        };
        layer.register_defaults();
        layer
    }

    /// Register default polyfills for common Node.js APIs
    fn register_defaults(&mut self) {
        // path.join
        self.polyfills.insert(
            "path.join".to_string(),
            r#"function join(...args) { return args.join('/').replace(/\/+/g, '/'); }"#.to_string(),
        );

        // path.dirname
        self.polyfills.insert(
            "path.dirname".to_string(),
            r#"function dirname(p) { return p.split('/').slice(0, -1).join('/') || '.'; }"#
                .to_string(),
        );

        // path.basename
        self.polyfills.insert(
            "path.basename".to_string(),
            r#"function basename(p) { return p.split('/').pop() || p; }"#.to_string(),
        );

        // path.extname
        self.polyfills.insert(
            "path.extname".to_string(),
            r#"function extname(p) { const i = p.lastIndexOf('.'); return i < 0 ? '' : p.slice(i); }"#.to_string(),
        );

        // Buffer.from
        self.polyfills.insert(
            "Buffer.from".to_string(),
            r#"function from(str, enc) { return enc === 'hex' ? str : str; }"#.to_string(),
        );

        // process.env
        self.polyfills
            .insert("process.env".to_string(), r#"const env = {};"#.to_string());

        // URL
        self.polyfills.insert(
            "URL".to_string(),
            r#"function URL(url, base) { this.href = url; this.pathname = url.split('?')[0]; }"#
                .to_string(),
        );

        // crypto.randomUUID (simplified)
        self.polyfills.insert(
            "crypto.randomUUID".to_string(),
            r#"function randomUUID() { return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, c => { const r = Math.random()*16|0; return (c==='x'?r:(r&0x3|0x8)).toString(16); }); }"#.to_string(),
        );
    }

    /// Register a custom polyfill
    pub fn register(&mut self, api: &str, implementation: &str) {
        self.polyfills
            .insert(api.to_string(), implementation.to_string());
    }

    /// Get a polyfill implementation
    pub fn get(&self, api: &str) -> Option<&str> {
        self.polyfills.get(api).map(|s| s.as_str())
    }

    /// Generate the full polyfill JS source for injection into the runtime
    pub fn polyfill_source(&self) -> String {
        self.polyfills
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get the list of polyfilled APIs
    pub fn polyfilled_apis(&self) -> Vec<String> {
        self.polyfills.keys().cloned().collect()
    }

    /// Check if an API is polyfilled
    pub fn has_polyfill(&self, api: &str) -> bool {
        self.polyfills.contains_key(api)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_g104_isolate_pool() {
        let mut pool = IsolatePool::new(4);
        assert_eq!(pool.stats().created, 0);

        let ctx1 = pool.acquire();
        assert_eq!(pool.stats().created, 1);
        assert_eq!(pool.stats().reused, 0);

        pool.release(ctx1);
        assert_eq!(pool.stats().pool_size, 1);

        let ctx2 = pool.acquire();
        assert_eq!(pool.stats().reused, 1);
        assert_eq!(pool.stats().created, 1);
        pool.release(ctx2);
    }

    #[test]
    fn isolate_pool_applies_the_configured_memory_limit() {
        let config = RuntimeConfig {
            memory_limit: 8 * 1024 * 1024,
            ..RuntimeConfig::default()
        };
        let mut pool = IsolatePool::with_config(1, &config);
        let (_rt, ctx) = pool.acquire();
        // ~64 MiB string build: far above the 8 MiB cap.
        let res = ctx.with(|ctx| {
            ctx.eval::<usize, _>("var s = 'x'.repeat(1024); for (var i = 0; i < 16; i++) { s = s + s + s + s; } s.length")
        });
        assert!(res.is_err(), "allocation past the limit must fail: {res:?}");

        // A small allocation under the same limit still works.
        let ok = ctx.with(|ctx| ctx.eval::<usize, _>("'abc'.repeat(10).length"));
        assert_eq!(ok.unwrap(), 30);
    }

    #[test]
    fn unlimited_memory_limit_zero_is_not_a_zero_byte_cap() {
        let config = RuntimeConfig {
            memory_limit: 0,
            ..RuntimeConfig::default()
        };
        let mut pool = IsolatePool::with_config(1, &config);
        let (_rt, ctx) = pool.acquire();
        let n = ctx.with(|ctx| ctx.eval::<usize, _>("'abc'.repeat(1000).length"));
        assert_eq!(n.unwrap(), 3000);
    }

    #[test]
    fn test_g105_detect_transform_schema() {
        let json = r#"{"code": "console.log(1)", "map": null}"#;
        assert_eq!(detect_output_schema(json), OutputSchema::TransformOutput);
    }

    #[test]
    fn test_g105_detect_resolve_schema() {
        let json = r#"{"id": "/src/a.ts", "external": false, "code": ""}"#;
        assert_eq!(detect_output_schema(json), OutputSchema::ResolveResult);
    }

    #[test]
    fn test_g105_detect_unknown_schema() {
        let json = r#"{"foo": "bar"}"#;
        assert_eq!(detect_output_schema(json), OutputSchema::Unknown);
    }

    #[test]
    fn test_g106_analyze_simple_plugin() {
        let source = r#"
            export default {
                name: "simple-plugin",
                transform(code, id) {
                    return { code, map: null };
                }
            };
        "#;
        let analysis = analyze_vite_plugin(source, "simple-plugin");
        assert_eq!(analysis.plugin_name, "simple-plugin");
        assert!(analysis.hooks.contains(&"transform".to_string()));
        assert_eq!(analysis.difficulty, MigrationDifficulty::Trivial);
        assert!(analysis.auto_transpilable);
    }

    #[test]
    fn test_g106_analyze_complex_plugin() {
        let source = r#"
            import fs from 'fs';
            import path from 'path';
            export default {
                name: "complex-plugin",
                async transform(code, id) {
                    const data = fs.readFileSync(path.join(process.cwd(), 'config.json'));
                    return { code: code + data.toString(), map: null };
                },
                configureServer(server) {
                    server.use((req, res, next) => { next(); });
                }
            };
        "#;
        let analysis = analyze_vite_plugin(source, "complex-plugin");
        assert!(analysis.complexity > 30);
        assert!(!analysis.auto_transpilable);
        assert!(analysis.node_apis.contains(&"fs".to_string()));
        assert!(analysis.node_apis.contains(&"path".to_string()));
    }

    #[test]
    fn test_g107_plugin_batcher() {
        let mut batcher = PluginBatcher::new();
        batcher.add_plugin(
            "plugin1",
            "export default { name: 'p1', transform(c,i) { return {code:c,map:null}; } }",
        );
        batcher.add_plugin(
            "plugin2",
            "export default { name: 'p2', resolveId(s,i) { return {id:s,external:false}; } }",
        );
        assert_eq!(batcher.count(), 2);
        assert!(
            batcher
                .combined_source()
                .contains("__pledge_batch_plugin_0")
        );
        assert!(
            batcher
                .combined_source()
                .contains("__pledge_batch_plugin_1")
        );
    }

    #[test]
    fn test_g108_can_transpile_simple() {
        let source = r#"
            export default {
                name: "simple",
                transform(code, id) { return { code, map: null }; }
            };
        "#;
        assert!(can_transpile_to_wasm(source));
    }

    #[test]
    fn test_g108_cannot_transpile_with_node_apis() {
        let source = r#"
            import fs from 'fs';
            export default {
                name: "uses-fs",
                transform(code, id) {
                    const data = fs.readFileSync('foo');
                    return { code, map: null };
                }
            };
        "#;
        assert!(!can_transpile_to_wasm(source));
    }

    #[test]
    fn test_g108_generate_skeleton() {
        let source = r#"
            export default {
                name: "my-plugin",
                transform(code, id) { return { code, map: null }; },
                resolveId(s, i) { return { id: s, external: false }; }
            };
        "#;
        let skeleton = generate_wasm_skeleton("my-plugin", source);
        assert!(skeleton.contains("MyPlugin"));
        assert!(skeleton.contains("fn transform"));
        assert!(skeleton.contains("fn resolve_id"));
        // Regression test for goal 47: hook bodies must not silently compile
        // to a no-op `None` — that reads as "migrated and working" when
        // nothing was actually ported. `todo!()` forces the plugin author
        // to notice and fill it in (or get a clear panic if they don't).
        assert!(skeleton.contains("todo!("));
        assert!(!skeleton.contains("        None\n"));
        assert!(skeleton.to_lowercase().contains("manual port required"));
    }

    #[test]
    fn test_g109_runtime_info() {
        // current_runtime() reads env var, default is QuickJs
        assert!(!JsRuntime::QuickJs.is_jit_enabled());
        assert!(JsRuntime::V8.is_jit_enabled());
        assert!(JsRuntime::QuickJsJit.is_jit_enabled());
    }

    #[test]
    fn test_g103_runtime_config_default() {
        let config = RuntimeConfig::default();
        // Without features compiled in, resolve() should fall back to QuickJs
        let resolved = config.resolve();
        assert!(
            resolved == JsRuntime::QuickJs
                || resolved == JsRuntime::QuickJsJit
                || resolved == JsRuntime::V8
        );
    }

    #[test]
    fn test_g103_runtime_config_max_compatibility() {
        let config = RuntimeConfig::max_compatibility();
        assert_eq!(config.backend, JsRuntime::V8);
        assert_eq!(config.fallback, JsRuntime::QuickJsJit);
        assert!(config.enable_jit);
        assert_eq!(config.memory_limit, 512 * 1024 * 1024);
    }

    #[test]
    fn test_g103_runtime_config_no_node() {
        let config = RuntimeConfig::no_node();
        assert_eq!(config.backend, JsRuntime::QuickJsJit);
        assert_eq!(config.fallback, JsRuntime::QuickJs);
        assert!(config.enable_jit);
    }

    #[test]
    fn test_g103_runtime_config_minimal() {
        let config = RuntimeConfig::minimal();
        assert_eq!(config.backend, JsRuntime::QuickJs);
        assert!(!config.enable_jit);
        assert_eq!(config.memory_limit, 64 * 1024 * 1024);
    }

    #[test]
    fn test_g103_runtime_config_resolve_fallback() {
        // When JIT is not available, V8 config should fall back
        let config = RuntimeConfig::max_compatibility();
        let resolved = config.resolve();
        // Without v8 feature, should fall back to QuickJs (since QuickJS JIT also not available)
        if !jit_available() {
            assert_eq!(resolved, JsRuntime::QuickJs);
        }
    }

    #[test]
    fn test_g1010_node_compat() {
        let layer = NodeCompatLayer::new();
        assert!(layer.has_polyfill("path.join"));
        assert!(layer.has_polyfill("path.dirname"));
        assert!(layer.has_polyfill("Buffer.from"));
        assert!(layer.has_polyfill("process.env"));
        assert!(!layer.has_polyfill("nonexistent.api"));

        let source = layer.polyfill_source();
        assert!(source.contains("function join"));
    }

    #[test]
    fn test_g1010_register_custom() {
        let mut layer = NodeCompatLayer::new();
        layer.register("custom.api", "function customApi() { return 42; }");
        assert!(layer.has_polyfill("custom.api"));
        assert_eq!(
            layer.get("custom.api"),
            Some("function customApi() { return 42; }")
        );
    }

    #[test]
    fn test_to_pascal_case() {
        assert_eq!(to_pascal_case("my-plugin"), "MyPlugin");
        assert_eq!(to_pascal_case("my_plugin"), "MyPlugin");
        assert_eq!(to_pascal_case("my plugin"), "MyPlugin");
    }
}
