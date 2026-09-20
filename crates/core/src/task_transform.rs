// Task-graph-based transform pipeline.
//
// This module wraps the existing transform functions in PledgePack's task graph
// system (pledgepack-task-system). It introduces two task types:
//
// 1. **Parse task** (`Task<ParsedModule>`): Parses source code and extracts
//    a serializable summary (imports, exports, dynamic imports). This is the
//    "parse once" boundary — the parse result is cached by TaskId, so
//    unchanged modules skip re-parsing entirely.
//
// 2. **Transform task** (`Task<TransformTaskOutput>`): Depends on the parse
//    task. Performs the full transform (JSX → JS, TS stripping, minification,
//    codegen). The output is a serializable version of TransformOutput.
//
// The AST itself (oxc::ast::ast::Program) is arena-allocated and not
// serializable, so it lives only during the transform task's execution.
// The parse task stores the *summary* (imports/exports/dynamic imports),
// which is what downstream consumers (the build engine, code splitting)
// actually need. This is the same approach as turbo-tasks' `Vc<Module>`
// but without the 9 wrapper types.
//
// Plugin integration: JsPluginHost hooks (resolveId, load, transform) are
// callable from the transform pipeline. Plugin transform outputs are cached
// as part of the transform task, so plugin results are memoized.

use crate::config::PledgeConfig;
use crate::module::ModuleKind;
use crate::transform::{self, TransformOutput};
use pledgepack_task_system::{
    StoredOutput, Task, TaskEngine, TaskEngineBuilder, TaskExecutor, TaskId, TaskRegistry,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, trace};

// ─── Serializable task outputs ────────────────────────────────────────

/// The result of a plugin transform hook.
///
/// This is a trait-based indirection so that `task_transform.rs` doesn't
/// depend on `pledgepack-js-plugin-host` directly (which would create a
/// circular dependency). The js-plugin-host crate constructs this from
/// its `TransformResult` type.
/// Shared callback type for plugin transform hooks — the indirection that
/// lets this module avoid a circular dependency on the JS/WASM plugin hosts.
pub type PluginTransformFn = Arc<dyn Fn(&str, &str) -> Option<PluginTransformResult> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct PluginTransformResult {
    /// The transformed code
    pub code: String,
    /// Optional source map
    pub map: Option<String>,
    /// Plugin-provided cache key for fine-grained invalidation.
    ///
    /// For WASM plugins: this is the `cache_key` field from the WIT
    /// `transform-output` record. The plugin computes it from its inputs.
    ///
    /// For JS plugins: this is the coarse cache key (blake3 of inputs +
    /// plugin paths), computed by `JsPluginHostBridge::coarse_cache_key()`.
    ///
    /// This is mixed into the TaskId so that if the plugin's cache key
    /// changes, the task is re-computed.
    pub cache_key: Option<String>,
}

/// Output of a file read task (Item 4: Zig hot paths).
///
/// File I/O is wrapped as a task in the task graph, enabling:
/// - Caching: unchanged files skip re-reading
/// - Future io_uring: the task executor can use io_uring on Linux
/// - SIMD scanning: import detection happens inside the task
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileReadOutput {
    /// Raw file content (bytes)
    pub source: Vec<u8>,
    /// File content as string (lossy UTF-8)
    pub source_str: String,
    /// Import specifiers found by SIMD scanning
    pub imports: Vec<String>,
    /// Content hash for cache invalidation
    pub content_hash: u64,
}

/// A serializable summary of parsing a module.
///
/// This is what `Task<ParsedModule>` stores. The full AST is not serializable
/// (it's arena-allocated), so we store the summary that downstream consumers
/// need: imports, exports, dynamic imports, and a source hash for validation.
///
/// The `ast_handle` field is the bridge to the `AstPool`: it's the content hash
/// of the source, which can be used to look up the pre-parsed AST in the pool.
/// When the task is loaded from cache (disk/remote), the `ast_handle` is still
/// valid — it can be used to re-parse and re-cache the AST on demand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedModule {
    /// Static import specifiers (e.g., "./foo", "react")
    pub imports: Vec<String>,
    /// Export names (for tree shaking)
    pub exports: Vec<String>,
    /// Dynamic import specifiers (for code splitting)
    pub dynamic_imports: Vec<String>,
    /// Whether the module has a default export
    pub has_default_export: bool,
    /// Blake3 hash of the source (for cache validation)
    pub source_hash: [u8; 32],
    /// The module kind
    pub kind: ModuleKind,
    /// File path (for diagnostics)
    pub file_path: String,
    /// Phase 4: AstPool handle (FNV-1a content hash of the source).
    /// Used to look up the pre-parsed AST in the `AstPool`.
    /// When the task is restored from cache, this handle can be used to
    /// re-parse and re-cache the AST on demand.
    pub ast_handle: u64,
}

/// A serializable version of `TransformOutput` for the task graph.
///
/// This is what `Task<TransformTaskOutput>` stores. It mirrors
/// `crate::transform::TransformOutput` but derives serde for caching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformTaskOutput {
    pub code: String,
    pub source_map: Option<String>,
    pub css_modules: Option<Vec<(String, String)>>,
    pub is_css: bool,
    pub extracted_css: Option<String>,
    pub is_worker: bool,
    pub dynamic_imports: Vec<String>,
    pub content_hash: Option<String>,
    /// Dependencies discovered during transform (for graph wiring)
    pub deps: Vec<String>,
    /// Plugin-applied flag: whether a JS plugin transformed this module
    pub plugin_transformed: bool,
}

/// G7.2: A plugin hook output stored as a first-class `Task<PluginOutput>` node.
///
/// Each plugin hook (resolve-id, load, transform, transform-index-html, render-chunk)
/// returns a `cache_key` (blake3 hash of inputs). This type wraps the output
/// as a serializable task graph node, keyed by `TaskId = blake3("plugin_output" ++ cache_key)`.
///
/// This gives WASM plugins fine-grained caching: if the same plugin is called
/// with the same inputs (same cache_key), the task graph returns the cached
/// output without re-invoking the plugin.
///
/// For JS plugins (second-class tier), the `cache_key` is a coarse hash
/// (blake3 of inputs + plugin paths), so caching is less precise but still
/// functional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginOutput {
    /// The plugin name (for diagnostics)
    pub plugin_name: String,
    /// Which hook produced this output
    pub hook: String,
    /// The plugin-provided cache key (blake3 hash of plugin inputs)
    pub cache_key: String,
    /// The transformed code (for transform/load/render-chunk hooks)
    pub code: Option<String>,
    /// Optional source map
    pub source_map: Option<String>,
    /// Resolved module ID (for resolve-id hook)
    pub resolved_id: Option<String>,
    /// Whether the resolved module is external (for resolve-id hook)
    pub external: Option<bool>,
    /// HTML tags (for transform-index-html hook)
    pub html_tags: Option<Vec<HtmlTagEntry>>,
    /// G7.6: Schema version of the plugin output
    pub schema_version: Option<u32>,
}

/// Serializable HTML tag entry (mirrors WIT `html-tag` record).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HtmlTagEntry {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub children: Option<String>,
    pub inject_to: Option<String>,
}

impl PluginOutput {
    /// Compute the `TaskId` for a plugin output from its cache key.
    ///
    /// The TaskId is `blake3("plugin_output" ++ plugin_name ++ hook ++ cache_key)`.
    /// This means:
    /// - Same plugin + same hook + same inputs → same TaskId → cache hit
    /// - Different plugins or hooks → different TaskIds → no collision
    /// - Changed inputs → different cache_key → different TaskId → recompute
    pub fn task_id(plugin_name: &str, hook: &str, cache_key: &str) -> TaskId {
        let mut input_bytes = Vec::new();
        input_bytes.extend_from_slice(plugin_name.as_bytes());
        input_bytes.push(0); // separator
        input_bytes.extend_from_slice(hook.as_bytes());
        input_bytes.push(0); // separator
        input_bytes.extend_from_slice(cache_key.as_bytes());
        TaskId::compute("plugin_output", &input_bytes)
    }

    /// Create a PluginOutput from a transform hook result.
    pub fn from_transform(
        plugin_name: &str,
        cache_key: &str,
        code: String,
        source_map: Option<String>,
    ) -> Self {
        Self {
            plugin_name: plugin_name.to_string(),
            hook: "transform".to_string(),
            cache_key: cache_key.to_string(),
            code: Some(code),
            source_map,
            resolved_id: None,
            external: None,
            html_tags: None,
            schema_version: Some(1),
        }
    }

    /// Create a PluginOutput from a resolve-id hook result.
    pub fn from_resolve_id(plugin_name: &str, cache_key: &str, id: String, external: bool) -> Self {
        Self {
            plugin_name: plugin_name.to_string(),
            hook: "resolve-id".to_string(),
            cache_key: cache_key.to_string(),
            code: None,
            source_map: None,
            resolved_id: Some(id),
            external: Some(external),
            html_tags: None,
            schema_version: Some(1),
        }
    }

    /// Create a PluginOutput from a load hook result.
    pub fn from_load(
        plugin_name: &str,
        cache_key: &str,
        code: String,
        source_map: Option<String>,
    ) -> Self {
        Self {
            plugin_name: plugin_name.to_string(),
            hook: "load".to_string(),
            cache_key: cache_key.to_string(),
            code: Some(code),
            source_map,
            resolved_id: None,
            external: None,
            html_tags: None,
            schema_version: Some(1),
        }
    }
}

impl From<TransformOutput> for TransformTaskOutput {
    fn from(o: TransformOutput) -> Self {
        Self {
            code: o.code,
            source_map: o.source_map,
            css_modules: o.css_modules,
            is_css: o.is_css,
            extracted_css: o.extracted_css,
            is_worker: o.is_worker,
            dynamic_imports: o.dynamic_imports.clone(),
            content_hash: o.content_hash,
            deps: o.dynamic_imports, // Will be overwritten with actual deps
            plugin_transformed: false,
        }
    }
}

impl From<TransformTaskOutput> for TransformOutput {
    fn from(o: TransformTaskOutput) -> Self {
        Self {
            code: o.code,
            source_map: o.source_map,
            css_modules: o.css_modules,
            is_css: o.is_css,
            extracted_css: o.extracted_css,
            is_worker: o.is_worker,
            dynamic_imports: o.dynamic_imports,
            content_hash: o.content_hash,
        }
    }
}

// ─── Task-based transform engine ──────────────────────────────────────

/// A task-graph-backed transform engine.
///
/// Wraps the existing `transform::transform()` function in two task layers:
///
/// 1. **Parse**: `parse_task(source, kind, file_path)` → `Task<ParsedModule>`
///    - Extracts imports, exports, dynamic imports via SIMD + AST scan
///    - Cached by content hash: unchanged modules skip parsing
///
/// 2. **Transform**: `transform_task(source, kind, file_path, is_production, config)`
///    → `Task<TransformTaskOutput>`
///    - Depends on the parse task (registers the dependency edge)
///    - Calls `transform::transform()` for the actual transform
///    - Plugin transform hooks are applied here (if plugin host is set)
///
/// The engine owns a `TaskEngine` and `TaskRegistry`. It's designed to be
/// used alongside the existing `BuildEngine` — the build engine can delegate
/// transform calls to this engine, getting task-graph caching for free.
pub struct TaskTransformEngine {
    engine: TaskEngine,
}

impl TaskTransformEngine {
    /// Create a new task transform engine with memory-only caching.
    pub fn new() -> Self {
        let registry = TaskRegistry::new();
        let engine = TaskEngineBuilder::new(registry).build();
        Self { engine }
    }

    /// Create a new task transform engine with disk caching.
    ///
    /// Uses the content-addressed store (`<cache_dir>/cas/`): task outputs
    /// are zstd-compressed blobs named by their blake3, indexed by an
    /// append-only log — identical outputs dedupe automatically and reads
    /// are integrity-verified by construction.
    pub fn with_disk(cache_dir: std::path::PathBuf) -> std::io::Result<Self> {
        let registry = TaskRegistry::new();
        let cas = pledgepack_task_system::CasBackend::new(cache_dir)?;
        let engine = TaskEngineBuilder::new(registry).with_cas(cas).build();
        Ok(Self { engine })
    }

    /// Wrap an externally-built `TaskEngine` — e.g. one constructed via
    /// `TaskEngineBuilder` with disk + remote tiers configured by the
    /// caller (as `BuildEngine` does from `config.cache`).
    pub fn from_engine(engine: TaskEngine) -> Self {
        Self { engine }
    }

    /// Create a new task transform engine with disk caching and determinism verification.
    pub fn with_disk_and_verify_determinism(
        cache_dir: std::path::PathBuf,
    ) -> std::io::Result<Self> {
        let registry = TaskRegistry::new();
        let cas = pledgepack_task_system::CasBackend::new(cache_dir)?;
        let engine = TaskEngineBuilder::new(registry)
            .with_cas(cas)
            .with_verify_determinism()
            .build();
        Ok(Self { engine })
    }

    /// Compute the TaskId for a parse task.
    ///
    /// This is deterministic: same (source, kind, file_path) → same TaskId.
    /// The build engine can use this to check if a module is already cached
    /// without running the task.
    pub fn parse_task_id(source: &str, kind: ModuleKind, file_path: &str) -> TaskId {
        let mut input_bytes = Vec::new();
        input_bytes.extend_from_slice(source.as_bytes());
        input_bytes.push(kind as u8);
        input_bytes.extend_from_slice(file_path.as_bytes());
        TaskId::compute("parse_module", &input_bytes)
    }

    /// Compute the TaskId for a transform task.
    ///
    /// This includes the parse task's ID as a dependency, so changing the
    /// source (which changes the parse TaskId) automatically invalidates
    /// the transform task.
    pub fn transform_task_id(
        source: &str,
        kind: ModuleKind,
        file_path: &str,
        is_production: bool,
        config_hash: u64,
    ) -> TaskId {
        let mut input_bytes = Vec::new();
        input_bytes.extend_from_slice(source.as_bytes());
        input_bytes.push(kind as u8);
        input_bytes.extend_from_slice(file_path.as_bytes());
        input_bytes.push(is_production as u8);
        input_bytes.extend_from_slice(&config_hash.to_le_bytes());
        TaskId::compute("transform_module", &input_bytes)
    }

    /// Compute the TaskId for a transform task WITH a plugin cache key.
    ///
    /// This is the plugin-aware variant of `transform_task_id()`. The
    /// `plugin_cache_key` is mixed into the hash, so different plugin
    /// configurations produce different cache entries.
    ///
    /// For JS plugins: the `plugin_cache_key` is the coarse cache key
    /// (blake3 of input + plugin paths).
    ///
    /// For WASM plugins: the `plugin_cache_key` is a hash of the plugin's
    /// metadata (name + version) + the plugin's declared cache key inputs.
    /// The plugin's own `cache_key` field (returned in TransformOutput) is
    /// used for invalidation — if it changes, this TaskId changes, and the
    /// task is re-computed.
    pub fn transform_task_id_with_plugin(
        source: &str,
        kind: ModuleKind,
        file_path: &str,
        is_production: bool,
        config_hash: u64,
        plugin_cache_key: &str,
    ) -> TaskId {
        let mut input_bytes = Vec::new();
        input_bytes.extend_from_slice(source.as_bytes());
        input_bytes.push(kind as u8);
        input_bytes.extend_from_slice(file_path.as_bytes());
        input_bytes.push(is_production as u8);
        input_bytes.extend_from_slice(&config_hash.to_le_bytes());
        // Separator to distinguish plugin cache key from other inputs
        input_bytes.push(0xFD);
        input_bytes.extend_from_slice(plugin_cache_key.as_bytes());
        TaskId::compute("transform_module_with_plugin", &input_bytes)
    }

    /// Compute the TaskId for a file read task.
    ///
    /// This enables file I/O to be cached in the task graph. If the file
    /// hasn't changed (same path + same content hash), the read task hits
    /// the cache and returns instantly.
    ///
    /// Item 4: Zig hot paths — file I/O is now a task, enabling future
    /// io_uring integration (the task executor can use io_uring on Linux
    /// for async file reads).
    pub fn read_file_task_id(file_path: &str) -> TaskId {
        TaskId::compute("read_file", file_path.as_bytes())
    }

    /// Register a file read task.
    ///
    /// Reads the file inside the task executor, using async I/O:
    /// - On Linux: uses io_uring via `tokio-uring` for true async file reads
    /// - On other platforms: uses `tokio::fs::read` (thread pool based)
    ///
    /// This moves file I/O into the task graph, enabling:
    /// - Caching: unchanged files skip re-reading
    /// - io_uring on Linux: async file I/O without thread pool overhead
    /// - SIMD scanning: import detection happens inside the task
    pub fn register_read_task(&self, file_path: Arc<String>) -> Task<FileReadOutput> {
        let task_id = Self::read_file_task_id(&file_path);

        let file_path_clone = file_path.clone();
        self.engine.registry().register(
            task_id,
            "read_file".to_string(),
            TaskExecutor::sync(move || {
                // Sync file read via the Zig fast path (native-sys has no
                // async read_file variant — the io_uring path was never
                // implemented there).
                let source = pledgepack_native_sys::read_file(&file_path_clone)?;

                // SIMD scanning for imports (happens inside the task)
                let (_, import_offsets) = pledgepack_native_sys::summarize_module(&source);
                let source_str = String::from_utf8_lossy(&source).to_string();
                let mut imports = Vec::new();
                for offset in import_offsets {
                    if let Some(dep) = extract_module_specifier(&source_str, offset) {
                        imports.push(dep);
                    }
                }

                let output = FileReadOutput {
                    source: source.clone(),
                    source_str,
                    imports,
                    content_hash: u64::from_be_bytes(
                        blake3::hash(&source).as_bytes()[0..8].try_into().unwrap(),
                    ),
                };

                Ok(StoredOutput::new(task_id, &output, vec![])?)
            }),
        );

        Task::from_id(task_id)
    }

    /// Read a file read task's output.
    pub async fn read_file_task(
        &self,
        task: Task<FileReadOutput>,
    ) -> anyhow::Result<Arc<FileReadOutput>> {
        task.read(&self.engine)
            .await
            .map_err(|e| anyhow::anyhow!("File read task failed: {:?}", e))
    }

    /// Register a parse task and return the Task<ParsedModule>.
    ///
    /// The parse task extracts imports, exports, and dynamic imports from
    /// the source. For JS/TS modules, it uses SIMD scanning (fast) + AST
    /// traversal (for exports). For other module kinds, it returns an
    /// empty summary.
    pub fn register_parse_task(
        &self,
        source: Arc<String>,
        kind: ModuleKind,
        file_path: Arc<String>,
    ) -> Task<ParsedModule> {
        let task_id = Self::parse_task_id(&source, kind, &file_path);

        // Register the executor (idempotent — registering the same task ID
        // twice just overwrites the executor)
        let source_clone = source.clone();
        let file_path_clone = file_path.clone();
        self.engine.registry().register(
            task_id,
            "parse_module".to_string(),
            TaskExecutor::sync(move || {
                let parsed = parse_module_sync(&source_clone, kind, &file_path_clone)?;
                Ok(StoredOutput::new(task_id, &parsed, vec![])?)
            }),
        );

        Task::from_id(task_id)
    }

    /// Register a transform task and return the Task<TransformTaskOutput>.
    ///
    /// The transform task depends on the parse task (the parse task's ID
    /// is recorded as a dependency). The transform calls
    /// `transform::transform()` for the actual work, then applies plugin
    /// transform hooks if a plugin host is provided.
    pub fn register_transform_task(
        &self,
        source: Arc<String>,
        kind: ModuleKind,
        file_path: Arc<String>,
        is_production: bool,
        config: Arc<PledgeConfig>,
        parse_task_id: TaskId,
    ) -> Task<TransformTaskOutput> {
        let task_id = Self::transform_task_id(
            &source,
            kind,
            &file_path,
            is_production,
            config_hash(&config),
        );

        let source_clone = source.clone();
        let file_path_clone = file_path.clone();
        let config_clone = config.clone();
        self.engine.registry().register(
            task_id,
            "transform_module".to_string(),
            TaskExecutor::sync(move || {
                let output = transform::transform(
                    &source_clone,
                    kind,
                    &file_path_clone,
                    is_production,
                    &config_clone,
                )?;

                // Convert to serializable form and extract deps
                let mut task_output = TransformTaskOutput::from(output);

                // Extract deps from the parsed module (imports)
                // We re-scan with SIMD here because the transform may have
                // changed the import structure (e.g., JSX → JS adds imports)
                let (_, import_offsets) =
                    pledgepack_native_sys::summarize_module(source_clone.as_bytes());
                let mut deps = Vec::new();
                for offset in import_offsets {
                    if let Some(dep) = extract_module_specifier(&source_clone, offset) {
                        deps.push(dep);
                    }
                }
                task_output.deps = deps;

                Ok(StoredOutput::new(
                    task_id,
                    &task_output,
                    vec![parse_task_id],
                )?)
            }),
        );

        Task::from_id(task_id)
    }

    /// Register a transform task with plugin support.
    ///
    /// After the built-in transform, the plugin transform hook is called
    /// via the provided `PluginTransformFn`. The plugin's output replaces
    /// the built-in output if the plugin returns a result.
    ///
    /// This uses a function pointer instead of a direct reference to
    /// `JsPluginHost` to avoid a circular dependency (js-plugin-host
    /// depends on core, not the other way around).
    #[allow(clippy::too_many_arguments)]
    pub fn register_transform_task_with_plugin(
        &self,
        source: Arc<String>,
        kind: ModuleKind,
        file_path: Arc<String>,
        is_production: bool,
        config: Arc<PledgeConfig>,
        parse_task_id: TaskId,
        plugin_transform: PluginTransformFn,
        plugin_cache_key: String,
    ) -> Task<TransformTaskOutput> {
        let task_id = Self::transform_task_id_with_plugin(
            &source,
            kind,
            &file_path,
            is_production,
            config_hash(&config),
            &plugin_cache_key,
        );

        let source_clone = source.clone();
        let file_path_clone = file_path.clone();
        let config_clone = config.clone();
        self.engine.registry().register(
            task_id,
            "transform_module_with_plugin".to_string(),
            TaskExecutor::sync(move || {
                // 1. Built-in transform
                let output = transform::transform(
                    &source_clone,
                    kind,
                    &file_path_clone,
                    is_production,
                    &config_clone,
                )?;

                let mut task_output = TransformTaskOutput::from(output);

                // 2. Apply plugin transform hook
                if let Some(plugin_result) = plugin_transform(&task_output.code, &file_path_clone) {
                    debug!(
                        "Plugin transformed {}: code {} → {} bytes",
                        file_path_clone,
                        task_output.code.len(),
                        plugin_result.code.len()
                    );
                    task_output.code = plugin_result.code;
                    if plugin_result.map.is_some() {
                        task_output.source_map = plugin_result.map;
                    }
                    task_output.plugin_transformed = true;
                }

                // 3. Extract deps
                let (_, import_offsets) =
                    pledgepack_native_sys::summarize_module(source_clone.as_bytes());
                let mut deps = Vec::new();
                for offset in import_offsets {
                    if let Some(dep) = extract_module_specifier(&source_clone, offset) {
                        deps.push(dep);
                    }
                }
                task_output.deps = deps;

                Ok(StoredOutput::new(
                    task_id,
                    &task_output,
                    vec![parse_task_id],
                )?)
            }),
        );

        Task::from_id(task_id)
    }

    /// Register a transform task with both pre and post plugin transforms.
    ///
    /// This supports plugin ordering (enforce: "pre"|"post"):
    /// - Pre-plugin runs BEFORE the built-in Oxc transform
    /// - Post-plugin runs AFTER the built-in Oxc transform
    ///
    /// Either closure can be `None` if no pre/post plugin is configured.
    #[allow(clippy::too_many_arguments)]
    pub fn register_transform_task_with_plugin_ordering(
        &self,
        source: Arc<String>,
        kind: ModuleKind,
        file_path: Arc<String>,
        is_production: bool,
        config: Arc<PledgeConfig>,
        parse_task_id: TaskId,
        pre_plugin: Option<PluginTransformFn>,
        post_plugin: Option<PluginTransformFn>,
        plugin_cache_key: String,
    ) -> Task<TransformTaskOutput> {
        let task_id = Self::transform_task_id_with_plugin(
            &source,
            kind,
            &file_path,
            is_production,
            config_hash(&config),
            &plugin_cache_key,
        );

        let source_clone = source.clone();
        let file_path_clone = file_path.clone();
        let config_clone = config.clone();
        self.engine.registry().register(
            task_id,
            "transform_module_with_plugin_ordering".to_string(),
            TaskExecutor::sync(move || {
                // 0. Apply pre-plugin transform (enforce: "pre")
                let effective_source = if let Some(ref pre) = pre_plugin {
                    if let Some(result) = pre(&source_clone, &file_path_clone) {
                        result.code
                    } else {
                        source_clone.as_ref().clone()
                    }
                } else {
                    source_clone.as_ref().clone()
                };

                // 1. Built-in transform
                let output = transform::transform(
                    &effective_source,
                    kind,
                    &file_path_clone,
                    is_production,
                    &config_clone,
                )?;

                let mut task_output = TransformTaskOutput::from(output);

                // 2. Apply post-plugin transform (enforce: "post" or default)
                if let Some(ref post) = post_plugin
                    && let Some(plugin_result) = post(&task_output.code, &file_path_clone)
                {
                    debug!(
                        "Post-plugin transformed {}: code {} → {} bytes",
                        file_path_clone,
                        task_output.code.len(),
                        plugin_result.code.len()
                    );
                    task_output.code = plugin_result.code;
                    if plugin_result.map.is_some() {
                        task_output.source_map = plugin_result.map;
                    }
                    task_output.plugin_transformed = true;
                }

                // 3. Extract deps
                let (_, import_offsets) =
                    pledgepack_native_sys::summarize_module(source_clone.as_bytes());
                let mut deps = Vec::new();
                for offset in import_offsets {
                    if let Some(dep) = extract_module_specifier(&source_clone, offset) {
                        deps.push(dep);
                    }
                }
                task_output.deps = deps;

                Ok(StoredOutput::new(
                    task_id,
                    &task_output,
                    vec![parse_task_id],
                )?)
            }),
        );

        Task::from_id(task_id)
    }

    /// Read a parse task's output.
    pub async fn read_parse(&self, task: Task<ParsedModule>) -> anyhow::Result<Arc<ParsedModule>> {
        task.read(&self.engine)
            .await
            .map_err(|e| anyhow::anyhow!("Parse task failed: {:?}", e))
    }

    /// Read a transform task's output.
    pub async fn read_transform(
        &self,
        task: Task<TransformTaskOutput>,
    ) -> anyhow::Result<Arc<TransformTaskOutput>> {
        task.read(&self.engine)
            .await
            .map_err(|e| anyhow::anyhow!("Transform task failed: {:?}", e))
    }

    /// Blocking variant of `read_transform` for rayon worker threads in
    /// `BuildEngine::transform_modules_via_tasks` — drives the same
    /// cache-check → compute flow without an async runtime.
    pub fn read_transform_blocking(
        &self,
        task: Task<TransformTaskOutput>,
    ) -> anyhow::Result<Arc<TransformTaskOutput>> {
        task.read_blocking(&self.engine)
            .map_err(|e| anyhow::anyhow!("Transform task failed: {:?}", e))
    }

    /// Blocking variant of `read_parse` for rayon worker threads.
    pub fn read_parse_blocking(
        &self,
        task: Task<ParsedModule>,
    ) -> anyhow::Result<Arc<ParsedModule>> {
        task.read_blocking(&self.engine)
            .map_err(|e| anyhow::anyhow!("Parse task failed: {:?}", e))
    }

    // ─── G7.2: Plugin outputs as first-class Task<T> nodes ──────────

    /// G7.2: Register a plugin output as a cached `Task<PluginOutput>` node.
    ///
    /// The plugin's `cache_key` (blake3 hash of plugin inputs) is used to
    /// compute a deterministic `TaskId`. If the same plugin + hook + cache_key
    /// combination has been seen before, the task graph returns the cached
    /// output without re-invoking the plugin.
    ///
    /// The caller computes the `PluginOutput` by invoking the plugin hook,
    /// then passes it here for caching. On subsequent calls with the same
    /// cache_key, the task graph returns the cached result.
    pub fn register_plugin_output_task(&self, output: PluginOutput) -> Task<PluginOutput> {
        let task_id = PluginOutput::task_id(&output.plugin_name, &output.hook, &output.cache_key);
        let output_clone = output.clone();

        self.engine.registry().register(
            task_id,
            format!("plugin_output:{}", output.hook),
            TaskExecutor::sync(move || Ok(StoredOutput::new(task_id, &output_clone, vec![])?)),
        );

        Task::from_id(task_id)
    }

    /// G7.2: Read a plugin output task from the task graph.
    ///
    /// Returns the cached `PluginOutput` if the task has been computed,
    /// or computes it if not yet cached.
    pub async fn read_plugin_output(
        &self,
        task: Task<PluginOutput>,
    ) -> anyhow::Result<Arc<PluginOutput>> {
        task.read(&self.engine)
            .await
            .map_err(|e| anyhow::anyhow!("Plugin output task failed: {:?}", e))
    }

    /// G7.2: Check if a plugin output is already cached (without computing).
    ///
    /// Returns `true` if the task graph has a cached result for this
    /// plugin + hook + cache_key combination.
    pub fn is_plugin_output_cached(&self, plugin_name: &str, hook: &str, cache_key: &str) -> bool {
        let task_id = PluginOutput::task_id(plugin_name, hook, cache_key);
        self.engine.is_cached(&task_id)
    }

    /// G7.2: Get the `TaskId` for a plugin output without registering it.
    ///
    /// Useful for checking cache existence or invalidation.
    pub fn plugin_output_task_id(plugin_name: &str, hook: &str, cache_key: &str) -> TaskId {
        PluginOutput::task_id(plugin_name, hook, cache_key)
    }

    /// Get the underlying TaskEngine (for cache inspection, stats, etc.)
    pub fn engine(&self) -> &TaskEngine {
        &self.engine
    }

    /// Get the task registry (for registering tasks)
    pub fn registry(&self) -> &Arc<TaskRegistry> {
        self.engine.registry()
    }

    /// Invalidate a task by its ID (e.g., when a file changes).
    pub fn invalidate(&self, task_id: TaskId) {
        self.engine.invalidate(task_id);
    }

    /// Phase 4: Bridge the task system to the AstPool.
    ///
    /// Given a `ParsedModule` (from a completed parse task) and a reference to
    /// the `AstPool`, ensure the pre-parsed AST is available in the pool.
    ///
    /// If the `ast_handle` is already in the pool (cache hit), this is a no-op.
    /// If not (cache miss — e.g., the task was restored from disk cache), the
    /// source is re-parsed and stored in the pool.
    ///
    /// Returns the `AstHandle` that can be used with `AstPool::with_program()`
    /// or `AstPool::take()`.
    ///
    /// This is the bridge between the serializable task graph (which stores
    /// `ParsedModule` with an `ast_handle: u64`) and the in-memory `AstPool`
    /// (which stores the actual arena-allocated `Program`).
    pub fn ensure_ast_in_pool(
        &self,
        parsed: &ParsedModule,
        source: &str,
        pool: &mut crate::ast_pool::AstPool,
    ) -> anyhow::Result<crate::ast_pool::AstHandle> {
        let handle = crate::ast_pool::AstHandle(parsed.ast_handle);

        // Determine the source type from the module kind + file path
        let path = std::path::Path::new(&parsed.file_path);
        let source_type =
            oxc::span::SourceType::from_path(path).unwrap_or_else(|_| match parsed.kind {
                ModuleKind::Tsx => oxc::span::SourceType::tsx(),
                ModuleKind::TypeScript => oxc::span::SourceType::ts(),
                ModuleKind::Jsx => oxc::span::SourceType::jsx(),
                _ => oxc::span::SourceType::mjs(),
            });

        // get_or_parse is a no-op if already cached, parses fresh if not
        pool.get_or_parse(source, source_type)
            .map_err(|e| anyhow::anyhow!("Failed to parse AST for pool: {}", e))?;

        Ok(handle)
    }
}

impl Default for TaskTransformEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Parse implementation ─────────────────────────────────────────────

/// Parse a module synchronously and return a serializable summary.
///
/// For JS/TS/JSX/TSX: uses SIMD scanning for imports + AST traversal for
/// exports and dynamic imports.
/// For other kinds: returns an empty summary (imports are extracted during
/// transform).
fn parse_module_sync(
    source: &str,
    kind: ModuleKind,
    file_path: &str,
) -> anyhow::Result<ParsedModule> {
    let source_hash = blake3::hash(source.as_bytes());

    // For non-JS modules, return a minimal summary
    if !matches!(
        kind,
        ModuleKind::TypeScript
            | ModuleKind::Tsx
            | ModuleKind::Jsx
            | ModuleKind::JavaScript
            | ModuleKind::Worker
            | ModuleKind::SharedWorker
    ) {
        return Ok(ParsedModule {
            imports: Vec::new(),
            exports: Vec::new(),
            dynamic_imports: Vec::new(),
            has_default_export: false,
            source_hash: source_hash.into(),
            kind,
            file_path: file_path.to_string(),
            ast_handle: crate::ast_pool::AstHandle::from_source(source).0,
        });
    }

    // SIMD scan for imports (fast — no AST needed)
    let (_, import_offsets) = pledgepack_native_sys::summarize_module(source.as_bytes());
    let mut imports = Vec::new();
    for offset in import_offsets {
        if let Some(dep) = extract_module_specifier(source, offset) {
            imports.push(dep);
        }
    }

    // AST traversal for exports and dynamic imports
    let (exports, has_default_export, dynamic_imports) =
        parse_exports_and_dynamic_imports(source, file_path);

    trace!(
        "Parsed {}: {} imports, {} exports, {} dynamic imports",
        file_path,
        imports.len(),
        exports.len(),
        dynamic_imports.len()
    );

    Ok(ParsedModule {
        imports,
        exports,
        dynamic_imports,
        has_default_export,
        source_hash: source_hash.into(),
        kind,
        file_path: file_path.to_string(),
        ast_handle: crate::ast_pool::AstHandle::from_source(source).0,
    })
}

/// Extract exports and dynamic imports via AST traversal.
///
/// Falls back to string-based detection if AST parsing fails.
fn parse_exports_and_dynamic_imports(
    source: &str,
    file_path: &str,
) -> (Vec<String>, bool, Vec<String>) {
    use oxc::allocator::Allocator;
    use oxc::ast_visit::Visit;
    use oxc::parser::{Parser, ParserReturn};
    use oxc::span::SourceType;

    let path = Path::new(file_path);
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());

    let allocator = Allocator::default();
    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, source, source_type).parse();

    if panicked {
        // Fallback: string-based detection
        let dynamic = string_detect_dynamic_imports(source);
        return (Vec::new(), false, dynamic);
    }

    struct AstCollector {
        exports: Vec<String>,
        has_default_export: bool,
        dynamic_imports: Vec<String>,
    }

    impl Visit<'_> for AstCollector {
        fn visit_export_named_declaration(&mut self, node: &oxc::ast::ast::ExportNamedDeclaration) {
            // Handle export { foo, bar }
            for specifier in &node.specifiers {
                self.exports.push(specifier.local.name().to_string());
            }
            // Handle export const foo = ..., export function foo() {}, etc.
            if let Some(decl) = &node.declaration {
                match decl {
                    oxc::ast::ast::Declaration::VariableDeclaration(var_decl) => {
                        for decl in &var_decl.declarations {
                            match &decl.id {
                                oxc::ast::ast::BindingPattern::BindingIdentifier(id) => {
                                    self.exports.push(id.name.to_string());
                                }
                                _ => {
                                    // Destructuring patterns — skip for simplicity
                                }
                            }
                        }
                    }
                    oxc::ast::ast::Declaration::FunctionDeclaration(fn_decl) => {
                        if let Some(id) = &fn_decl.id {
                            self.exports.push(id.name.to_string());
                        }
                    }
                    oxc::ast::ast::Declaration::ClassDeclaration(class_decl) => {
                        if let Some(id) = &class_decl.id {
                            self.exports.push(id.name.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        fn visit_export_default_declaration(
            &mut self,
            _node: &oxc::ast::ast::ExportDefaultDeclaration,
        ) {
            self.has_default_export = true;
        }

        fn visit_import_expression(&mut self, expr: &oxc::ast::ast::ImportExpression) {
            if let oxc::ast::ast::Expression::StringLiteral(lit) = &expr.source {
                let spec = &lit.value;
                if spec.starts_with("./") || spec.starts_with("../") {
                    self.dynamic_imports.push(spec.to_string());
                }
            }
        }
    }

    let mut collector = AstCollector {
        exports: Vec::new(),
        has_default_export: false,
        dynamic_imports: Vec::new(),
    };
    collector.visit_program(&program);

    (
        collector.exports,
        collector.has_default_export,
        collector.dynamic_imports,
    )
}

/// Fallback string-based dynamic import detection.
fn string_detect_dynamic_imports(source: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut search_pos = 0;

    while let Some(pos) = source[search_pos..].find("import(") {
        let abs_pos = search_pos + pos;
        let after = &source[abs_pos + 7..];

        if let Some(quote_pos) = after.find(['"', '\'']) {
            let quote_char = after.as_bytes()[quote_pos] as char;
            let spec_start = quote_pos + 1;
            let spec_rest = &after[spec_start..];
            if let Some(end) = spec_rest.find(quote_char) {
                let specifier = &spec_rest[..end];
                if specifier.starts_with("./") || specifier.starts_with("../") {
                    imports.push(specifier.to_string());
                }
            }
        }

        search_pos = abs_pos + 7;
    }

    imports
}

// ─── Helpers ──────────────────────────────────────────────────────────

/// Extract a module specifier from an import statement.
///
/// This is the same logic used in engine.rs but duplicated here to avoid
/// a circular dependency between the task transform module and the engine.
fn extract_module_specifier(source: &str, offset: usize) -> Option<String> {
    let rest = &source[offset..];
    if !rest.starts_with("import") && !rest.starts_with("export") {
        return None;
    }
    let keyword_len = 6;
    let after = rest[keyword_len..].trim_start();

    // import.meta — a builtin, not a dependency.
    if after.starts_with(".meta") {
        return None;
    }

    let first = after.chars().next()?;
    let dynamic = first == '(';
    if !dynamic {
        // Statement-position check: the match must start a statement. Scanning
        // back, a newline counts as a boundary (ASI); otherwise the previous
        // non-whitespace char must be `;`, `{`, or `}`.
        let mut at_boundary = source[..offset].trim().is_empty();
        for ch in source[..offset].chars().rev() {
            if ch == '\n' {
                at_boundary = true;
                break;
            }
            if ch.is_whitespace() {
                continue;
            }
            at_boundary = matches!(ch, ';' | '{' | '}');
            break;
        }
        if !at_boundary {
            return None;
        }
    }

    if !dynamic && first != '"' && first != '\'' {
        // Clause form: require a `from` token before the statement ends.
        let mut has_from = false;
        let mut cur = String::new();
        for ch in after.chars() {
            match ch {
                ';' | '<' | '>' => break,
                c if c.is_alphanumeric() || c == '_' || c == '$' => cur.push(c),
                _ => {
                    if cur == "from" {
                        has_from = true;
                        break;
                    }
                    cur.clear();
                }
            }
        }
        if cur == "from" {
            has_from = true;
        }
        if !has_from {
            return None;
        }
    }

    // Find the first string literal
    let mut in_string = false;
    let mut quote_char = '"';
    let mut specifier = String::new();

    for c in after.chars() {
        if !in_string && (c == '"' || c == '\'') {
            in_string = true;
            quote_char = c;
            specifier.clear();
        } else if in_string {
            if c == quote_char {
                if !specifier.is_empty() {
                    return Some(specifier);
                }
                in_string = false;
            } else {
                specifier.push(c);
            }
        } else if c == ';' {
            break;
        }
    }

    None
}

/// Compute a hash of the config for task ID computation.
///
/// This ensures that changing config options (e.g., production mode,
/// framework) invalidates transform tasks.
fn config_hash(config: &PledgeConfig) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Hash framework as a string (Framework doesn't impl Hash directly)
    format!("{:?}", config.framework).hash(&mut hasher);
    format!("{:?}", config.mode).hash(&mut hasher);
    config.source_maps.hash(&mut hasher);
    config.build.env_inline.hash(&mut hasher);
    if let Some(opt) = &config.optimize {
        opt.minify.hash(&mut hasher);
        opt.tree_shake.hash(&mut hasher);
    }
    // Hash define map deterministically (HashMap doesn't impl Hash)
    let mut define_keys: Vec<_> = config.define.keys().collect();
    define_keys.sort();
    for key in define_keys {
        key.hash(&mut hasher);
        config.define[key].hash(&mut hasher);
    }
    hasher.finish()
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_task_id_is_deterministic() {
        let id1 =
            TaskTransformEngine::parse_task_id("const x = 1;", ModuleKind::JavaScript, "test.js");
        let id2 =
            TaskTransformEngine::parse_task_id("const x = 1;", ModuleKind::JavaScript, "test.js");
        let id3 =
            TaskTransformEngine::parse_task_id("const y = 2;", ModuleKind::JavaScript, "test.js");
        let id4 =
            TaskTransformEngine::parse_task_id("const x = 1;", ModuleKind::TypeScript, "test.js");

        assert_eq!(id1, id2, "Same inputs → same TaskId");
        assert_ne!(id1, id3, "Different source → different TaskId");
        assert_ne!(id1, id4, "Different kind → different TaskId");
    }

    #[test]
    fn transform_task_id_is_deterministic() {
        let config = Arc::new(PledgeConfig::default());
        let ch = config_hash(&config);

        let id1 = TaskTransformEngine::transform_task_id(
            "const x = 1;",
            ModuleKind::JavaScript,
            "test.js",
            true,
            ch,
        );
        let id2 = TaskTransformEngine::transform_task_id(
            "const x = 1;",
            ModuleKind::JavaScript,
            "test.js",
            true,
            ch,
        );
        let id3 = TaskTransformEngine::transform_task_id(
            "const x = 1;",
            ModuleKind::JavaScript,
            "test.js",
            false,
            ch,
        );

        assert_eq!(id1, id2, "Same inputs → same TaskId");
        assert_ne!(id1, id3, "Different is_production → different TaskId");
    }

    #[tokio::test]
    async fn parse_task_extracts_imports() {
        let engine = TaskTransformEngine::new();
        let source = Arc::new(
            r#"
            import React from "react";
            import { useState } from "react";
            import { foo } from "./foo";
            export const bar = 42;
            export default function App() { return null; }
            const dyn = import("./dynamic");
            "#
            .to_string(),
        );

        let task =
            engine.register_parse_task(source, ModuleKind::Tsx, Arc::new("test.tsx".to_string()));
        let result = engine.read_parse(task).await.unwrap();

        assert!(result.imports.contains(&"./foo".to_string()));
        assert!(result.imports.contains(&"react".to_string()));
        assert!(result.exports.contains(&"bar".to_string()));
        assert!(result.has_default_export);
        assert!(result.dynamic_imports.contains(&"./dynamic".to_string()));
    }

    #[tokio::test]
    async fn transform_task_caches_output() {
        let engine = TaskTransformEngine::new();
        let source = Arc::new(
            r#"
            import React from "react";
            export const foo: number = 42;
            "#
            .to_string(),
        );

        let parse_task_id =
            TaskTransformEngine::parse_task_id(&source, ModuleKind::TypeScript, "test.ts");
        let parse_task = engine.register_parse_task(
            source.clone(),
            ModuleKind::TypeScript,
            Arc::new("test.ts".to_string()),
        );
        engine.read_parse(parse_task).await.unwrap();

        let config = Arc::new(PledgeConfig::default());
        let transform_task = engine.register_transform_task(
            source,
            ModuleKind::TypeScript,
            Arc::new("test.ts".to_string()),
            false,
            config,
            parse_task_id,
        );

        let result1 = engine.read_transform(transform_task).await.unwrap();
        assert!(result1.code.contains("42"));

        // Second read should hit cache
        let transform_task2 = engine.register_transform_task(
            Arc::new(r#"import React from "react"; export const foo: number = 42;"#.to_string()),
            ModuleKind::TypeScript,
            Arc::new("test.ts".to_string()),
            false,
            Arc::new(PledgeConfig::default()),
            parse_task_id,
        );
        let result2 = engine.read_transform(transform_task2).await.unwrap();
        assert_eq!(result1.code, result2.code);
    }

    #[test]
    fn extract_module_specifier_works() {
        assert_eq!(
            extract_module_specifier(r#"import React from "react";"#, 0),
            Some("react".to_string())
        );
        assert_eq!(
            extract_module_specifier(r#"import { x } from "./foo";"#, 0),
            Some("./foo".to_string())
        );
        let src = r#"const p = import("./dynamic");"#;
        let off = src.find("import").unwrap();
        assert_eq!(
            extract_module_specifier(src, off),
            Some("./dynamic".to_string())
        );

        // find_imports is a raw substring scan — `import` inside JSX text or
        // string literals must not produce bogus deps.
        let jsx_text =
            "<p>Debug meta tags, import maps, and script injection.</p>\n<div className=\"card\">";
        let off = jsx_text.find("import").unwrap();
        assert_eq!(extract_module_specifier(jsx_text, off), None);
        let in_string = "const s = 'auto-generates import maps for bare specifiers.';";
        let off = in_string.find("import").unwrap();
        assert_eq!(extract_module_specifier(in_string, off), None);
        assert_eq!(
            extract_module_specifier("export const x = 'value';", 0),
            None
        );
        assert_eq!(extract_module_specifier("import.meta.env", 0), None);
        let asi = "const x = 1\nimport y from 'z';";
        let off = asi.find("import").unwrap();
        assert_eq!(extract_module_specifier(asi, off), Some("z".to_string()));
    }

    #[tokio::test]
    async fn parsed_module_contains_ast_handle() {
        let engine = TaskTransformEngine::new();
        let source = Arc::new(
            r#"
            import React from "react";
            export const foo: number = 42;
            "#
            .to_string(),
        );

        let task = engine.register_parse_task(
            source.clone(),
            ModuleKind::TypeScript,
            Arc::new("test.ts".to_string()),
        );
        let result = engine.read_parse(task).await.unwrap();

        // The ast_handle should be the FNV-1a hash of the source
        let expected = crate::ast_pool::AstHandle::from_source(&source).0;
        assert_eq!(
            result.ast_handle, expected,
            "ast_handle should match FNV-1a hash of source"
        );
    }

    #[tokio::test]
    async fn ensure_ast_in_pool_bridges_task_to_pool() {
        let engine = TaskTransformEngine::new();
        let source = r#"
            import React from "react";
            export const foo: number = 42;
        "#;
        let source_arc = Arc::new(source.to_string());

        let task = engine.register_parse_task(
            source_arc.clone(),
            ModuleKind::TypeScript,
            Arc::new("test.ts".to_string()),
        );
        let parsed = engine.read_parse(task).await.unwrap();

        // The AstPool should be empty initially
        let mut pool = crate::ast_pool::AstPool::new();

        // Bridge: ensure the AST is in the pool
        let handle = engine
            .ensure_ast_in_pool(&parsed, &source_arc, &mut pool)
            .unwrap();
        assert_eq!(
            handle.0, parsed.ast_handle,
            "Handle should match the parsed module's ast_handle"
        );

        // The pool should now have the AST — we can read from it
        let imports = pool
            .with_program(handle, |prog| {
                crate::transform::detect_dynamic_imports_from_program(prog)
            })
            .unwrap_or_default();
        assert!(imports.is_empty(), "No dynamic imports in this source");
    }

    // ─── G7.2: Plugin output as Task<T> tests ──────────────────────

    #[test]
    fn plugin_output_task_id_is_deterministic() {
        let id1 = PluginOutput::task_id("my-plugin", "transform", "abc123");
        let id2 = PluginOutput::task_id("my-plugin", "transform", "abc123");
        assert_eq!(
            id1, id2,
            "Same plugin+hook+cache_key should produce same TaskId"
        );
    }

    #[test]
    fn plugin_output_task_id_differs_for_different_plugins() {
        let id1 = PluginOutput::task_id("plugin-a", "transform", "abc123");
        let id2 = PluginOutput::task_id("plugin-b", "transform", "abc123");
        assert_ne!(
            id1, id2,
            "Different plugins should produce different TaskIds"
        );
    }

    #[test]
    fn plugin_output_task_id_differs_for_different_hooks() {
        let id1 = PluginOutput::task_id("my-plugin", "transform", "abc123");
        let id2 = PluginOutput::task_id("my-plugin", "load", "abc123");
        assert_ne!(id1, id2, "Different hooks should produce different TaskIds");
    }

    #[test]
    fn plugin_output_task_id_differs_for_different_cache_keys() {
        let id1 = PluginOutput::task_id("my-plugin", "transform", "abc123");
        let id2 = PluginOutput::task_id("my-plugin", "transform", "def456");
        assert_ne!(
            id1, id2,
            "Different cache_keys should produce different TaskIds"
        );
    }

    #[tokio::test]
    async fn plugin_output_task_caches_result() {
        let engine = TaskTransformEngine::new();

        let task = engine.register_plugin_output_task(PluginOutput::from_transform(
            "test-plugin",
            "cache-key-1",
            "transformed code".to_string(),
            None,
        ));

        // First read computes
        let result1 = engine.read_plugin_output(task).await.unwrap();
        assert_eq!(result1.code.as_deref(), Some("transformed code"));

        // Second read should hit cache (same output)
        let result2 = engine.read_plugin_output(task).await.unwrap();
        assert_eq!(result2.code, result1.code);
    }

    #[tokio::test]
    async fn plugin_output_task_different_cache_keys_compute_separately() {
        let engine = TaskTransformEngine::new();

        let task1 = engine.register_plugin_output_task(PluginOutput::from_transform(
            "test-plugin",
            "key-1",
            "output-1".to_string(),
            None,
        ));
        let task2 = engine.register_plugin_output_task(PluginOutput::from_transform(
            "test-plugin",
            "key-2",
            "output-2".to_string(),
            None,
        ));

        let result1 = engine.read_plugin_output(task1).await.unwrap();
        let result2 = engine.read_plugin_output(task2).await.unwrap();

        assert_eq!(result1.code.as_deref(), Some("output-1"));
        assert_eq!(result2.code.as_deref(), Some("output-2"));
    }

    #[test]
    fn plugin_output_from_resolve_id() {
        let output =
            PluginOutput::from_resolve_id("resolver", "key", "/src/mod.tsx".to_string(), false);
        assert_eq!(output.hook, "resolve-id");
        assert_eq!(output.resolved_id.as_deref(), Some("/src/mod.tsx"));
        assert_eq!(output.external, Some(false));
        assert!(output.code.is_none());
    }

    #[test]
    fn plugin_output_from_load() {
        let output = PluginOutput::from_load(
            "loader",
            "key",
            "module code".to_string(),
            Some("map".to_string()),
        );
        assert_eq!(output.hook, "load");
        assert_eq!(output.code.as_deref(), Some("module code"));
        assert_eq!(output.source_map.as_deref(), Some("map"));
    }

    #[test]
    fn plugin_output_is_cached_check() {
        let engine = TaskTransformEngine::new();

        // Not cached yet
        assert!(!engine.is_plugin_output_cached("plugin", "transform", "key"));

        // Register and compute
        let task = engine.register_plugin_output_task(PluginOutput::from_transform(
            "plugin",
            "key",
            "code".to_string(),
            None,
        ));

        // Need to actually compute it for it to be cached
        // Use a runtime to drive the async read
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            engine.read_plugin_output(task).await.unwrap();
        });

        // Now it should be cached
        assert!(engine.is_plugin_output_cached("plugin", "transform", "key"));
    }
}
