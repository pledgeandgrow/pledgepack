// Plugin System enhancements
//
// Features:
//   38. Plugin hot reload — reload JS plugins without restarting dev server
//   39. Plugin sandboxing improvements — WASM memory/CPU/fs limits
//   40. Plugin dependency resolution — npm packages in WASM sandbox
//   41. Plugin lifecycle hooks — watchStart/watchChange/watchEnd
//   42. Plugin parallel execution via rayon

use dashmap::DashMap;
use ed25519_dalek::{Signature, VerifyingKey};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use subtle::{Choice, ConstantTimeEq};

// ─── Hook capability matrix ────────────────────────────────────────────
//
// The single source of truth for "which hooks does the plugin ABI define,
// by name" — both `pledgepack-wasm-plugin-host` and
// `pledgepack-js-plugin-host` reference this (rather than each hardcoding
// their own copy of the hook name list) so their `host_supports_hook`
// capability-matrix functions can't drift out of sync with each other or
// with `wit/world.wit`, and so a plugin author or PledgePack itself can
// discover "does this host actually run this hook" instead of it silently
// no-op'ing — which is exactly what happened to `buildStart`/`buildEnd`/
// `generateBundle` in the JS host before that was fixed (see
// PRODUCTION-READINESS-100.md goals 42, 45-46). This list must be kept in
// sync with `wit/world.wit`'s hook exports by hand — there's no automated
// generation from the WIT file today.
pub const PLUGIN_HOOK_NAMES: &[&str] = &[
    "resolveId",
    "load",
    "transform",
    "transformIndexHtml",
    "renderChunk",
    "handleHotUpdate",
    "buildStart",
    "buildEnd",
    "generateBundle",
    "configureServer",
];

// ─── Feature 38: Plugin hot reload ────────────────────────────────────

/// Tracks plugin source files for hot reload
pub struct PluginHotReloader {
    /// Map of plugin ID to source file path and last known hash
    plugin_sources: DashMap<String, PluginSource>,
    /// Callbacks to invoke when a plugin is reloaded
    reload_callbacks: DashMap<String, Vec<ReloadCallback>>,
    /// Optional debounced file watcher receiver (when using notify-debouncer)
    watcher_rx: DashMap<String, mpsc::Receiver<PathBuf>>,
}

#[derive(Debug, Clone)]
struct PluginSource {
    path: PathBuf,
    content_hash: [u8; 32],
    last_modified: std::time::SystemTime,
}

type ReloadCallback = Arc<dyn Fn(&str) + Send + Sync>;

impl PluginHotReloader {
    pub fn new() -> Self {
        Self {
            plugin_sources: DashMap::new(),
            reload_callbacks: DashMap::new(),
            watcher_rx: DashMap::new(),
        }
    }

    /// Register a plugin for hot reload watching
    pub fn register(&self, plugin_id: &str, source_path: &Path) {
        if let Ok(content) = std::fs::read(source_path) {
            let hash = blake3::hash(&content);
            let metadata = std::fs::metadata(source_path).ok();
            self.plugin_sources.insert(
                plugin_id.to_string(),
                PluginSource {
                    path: source_path.to_path_buf(),
                    content_hash: hash.into(),
                    last_modified: metadata
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(std::time::SystemTime::now()),
                },
            );
        }
    }

    /// Add a reload callback for a plugin
    pub fn on_reload(&self, plugin_id: &str, callback: impl Fn(&str) + Send + Sync + 'static) {
        let mut callbacks = self
            .reload_callbacks
            .entry(plugin_id.to_string())
            .or_default();
        callbacks.push(Arc::new(callback));
    }

    /// Check all registered plugins for changes and trigger reloads
    pub fn check_for_changes(&self) -> Vec<String> {
        let mut reloaded = Vec::new();

        let plugin_ids: Vec<String> = self
            .plugin_sources
            .iter()
            .map(|e| e.key().clone())
            .collect();

        for plugin_id in &plugin_ids {
            let source = match self.plugin_sources.get(plugin_id) {
                Some(s) => s,
                None => continue,
            };

            if let Ok(content) = std::fs::read(&source.path) {
                let new_hash = blake3::hash(&content);
                if new_hash.as_bytes() != &source.content_hash {
                    // Plugin source changed — trigger reload
                    if let Some(callbacks) = self.reload_callbacks.get(plugin_id) {
                        for cb in callbacks.iter() {
                            cb(plugin_id);
                        }
                    }

                    // Update stored hash
                    drop(source);
                    if let Some(mut source) = self.plugin_sources.get_mut(plugin_id) {
                        source.content_hash = new_hash.into();
                        if let Ok(metadata) = std::fs::metadata(&source.path)
                            && let Ok(modified) = metadata.modified()
                        {
                            source.last_modified = modified;
                        }
                    }

                    reloaded.push(plugin_id.clone());
                }
            }
        }

        reloaded
    }

    /// Unregister a plugin from hot reload watching
    pub fn unregister(&self, plugin_id: &str) {
        self.plugin_sources.remove(plugin_id);
        self.reload_callbacks.remove(plugin_id);
        self.watcher_rx.remove(plugin_id);
    }

    /// Start a notify-debouncer watcher for a plugin's source file.
    /// When the file changes, the callback is invoked automatically with
    /// built-in debouncing (replaces manual polling via `check_for_changes`).
    pub fn start_debounced_watcher(&self, plugin_id: &str, source_path: &Path) {
        use notify::RecursiveMode;
        use notify_debouncer_full::new_debouncer;

        let (tx, rx) = mpsc::channel::<PathBuf>();
        let path = source_path.to_path_buf();
        let plugin_id = plugin_id.to_string();

        let callback_plugin_id = plugin_id.clone();
        let callback_path = path.clone();

        let mut debouncer = match new_debouncer(
            Duration::from_millis(200),
            None,
            move |result: Result<
                Vec<notify_debouncer_full::DebouncedEvent>,
                Vec<notify::Error>,
            >| {
                if let Ok(events) = result {
                    for event in events {
                        for p in &event.paths {
                            if *p == callback_path {
                                let _ = tx.send(p.clone());
                            }
                        }
                    }
                }
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    "Failed to create debounced watcher for plugin {}: {}",
                    callback_plugin_id,
                    e
                );
                return;
            }
        };

        if let Err(e) = debouncer.watch(&path, RecursiveMode::NonRecursive) {
            tracing::warn!(
                "Failed to watch plugin source {}: {}",
                crate::display_path(&path),
                e
            );
            return;
        }

        // Keep the debouncer alive by leaking it (it runs in the background)
        // In a production system, we'd store the debouncer to drop it on unregister
        std::mem::forget(debouncer);

        self.watcher_rx.insert(plugin_id, rx);
    }

    /// Check for changes via debounced watchers (non-blocking).
    /// Returns list of reloaded plugin IDs.
    pub fn poll_debounced_changes(&self) -> Vec<String> {
        let mut reloaded = Vec::new();
        let plugin_ids: Vec<String> = self.watcher_rx.iter().map(|e| e.key().clone()).collect();

        for plugin_id in &plugin_ids {
            if let Some(rx) = self.watcher_rx.get_mut(plugin_id) {
                while let Ok(path) = rx.try_recv() {
                    // Trigger reload callbacks
                    if let Some(callbacks) = self.reload_callbacks.get(plugin_id) {
                        for cb in callbacks.iter() {
                            cb(plugin_id);
                        }
                    }
                    // Update stored hash
                    if let Ok(content) = std::fs::read(&path) {
                        let new_hash = blake3::hash(&content);
                        if let Some(mut source) = self.plugin_sources.get_mut(plugin_id) {
                            source.content_hash = new_hash.into();
                            if let Ok(metadata) = std::fs::metadata(&path)
                                && let Ok(modified) = metadata.modified()
                            {
                                source.last_modified = modified;
                            }
                        }
                    }
                    reloaded.push(plugin_id.clone());
                }
            }
        }
        reloaded
    }
}

impl Default for PluginHotReloader {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Feature 39: Plugin sandboxing improvements ───────────────────────

/// Resource limits for WASM plugin sandbox
#[derive(Debug, Clone)]
pub struct SandboxLimits {
    /// Maximum memory in bytes (default: 64MB)
    pub max_memory_bytes: usize,
    /// Maximum CPU time in milliseconds (default: 5000ms)
    pub max_cpu_time_ms: u64,
    /// Maximum number of filesystem reads
    pub max_fs_reads: usize,
    /// Maximum number of filesystem writes
    pub max_fs_writes: usize,
    /// Allowed filesystem paths (prefix matching)
    pub allowed_paths: Vec<PathBuf>,
    /// Whether network access is allowed
    pub allow_network: bool,
    /// Maximum stack depth for recursive calls
    pub max_stack_depth: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024, // 64MB
            max_cpu_time_ms: 5000,
            max_fs_reads: 100,
            max_fs_writes: 10,
            allowed_paths: vec![PathBuf::from("./src")],
            allow_network: false,
            max_stack_depth: 256,
        }
    }
}

/// Resource usage tracker for a sandboxed plugin
#[derive(Debug)]
pub struct ResourceUsage {
    pub memory_used: usize,
    pub cpu_time: Duration,
    pub fs_reads: usize,
    pub fs_writes: usize,
    pub start_time: Instant,
}

impl ResourceUsage {
    pub fn new() -> Self {
        Self {
            memory_used: 0,
            cpu_time: Duration::ZERO,
            fs_reads: 0,
            fs_writes: 0,
            start_time: Instant::now(),
        }
    }

    pub fn check_memory(&self, limits: &SandboxLimits) -> Result<(), SandboxError> {
        if self.memory_used > limits.max_memory_bytes {
            Err(SandboxError::MemoryLimitExceeded {
                used: self.memory_used,
                limit: limits.max_memory_bytes,
            })
        } else {
            Ok(())
        }
    }

    pub fn check_cpu_time(&self, limits: &SandboxLimits) -> Result<(), SandboxError> {
        let elapsed = self.start_time.elapsed();
        if elapsed.as_millis() as u64 > limits.max_cpu_time_ms {
            Err(SandboxError::CpuTimeLimitExceeded {
                used: elapsed,
                limit: Duration::from_millis(limits.max_cpu_time_ms),
            })
        } else {
            Ok(())
        }
    }

    pub fn check_fs_read(&self, limits: &SandboxLimits) -> Result<(), SandboxError> {
        if self.fs_reads >= limits.max_fs_reads {
            Err(SandboxError::FsReadLimitExceeded {
                count: self.fs_reads,
                limit: limits.max_fs_reads,
            })
        } else {
            Ok(())
        }
    }

    pub fn check_fs_write(&self, limits: &SandboxLimits) -> Result<(), SandboxError> {
        if self.fs_writes >= limits.max_fs_writes {
            Err(SandboxError::FsWriteLimitExceeded {
                count: self.fs_writes,
                limit: limits.max_fs_writes,
            })
        } else {
            Ok(())
        }
    }

    pub fn check_path_access(
        &self,
        path: &Path,
        limits: &SandboxLimits,
    ) -> Result<(), SandboxError> {
        let allowed = limits.allowed_paths.iter().any(|allowed| {
            path.starts_with(allowed)
                || path
                    .to_string_lossy()
                    .as_ref()
                    .contains(&allowed.to_string_lossy().as_ref().to_string())
        });
        if !allowed {
            Err(SandboxError::PathAccessDenied {
                path: path.to_path_buf(),
            })
        } else {
            Ok(())
        }
    }
}

impl Default for ResourceUsage {
    fn default() -> Self {
        Self::new()
    }
}

/// Sandbox errors
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("memory limit exceeded: used {used} bytes, limit {limit} bytes")]
    MemoryLimitExceeded { used: usize, limit: usize },
    #[error("CPU time limit exceeded: used {used:?}, limit {limit:?}")]
    CpuTimeLimitExceeded { used: Duration, limit: Duration },
    #[error("filesystem read limit exceeded: {count}/{limit}")]
    FsReadLimitExceeded { count: usize, limit: usize },
    #[error("filesystem write limit exceeded: {count}/{limit}")]
    FsWriteLimitExceeded { count: usize, limit: usize },
    #[error("path access denied: {path:?}")]
    PathAccessDenied { path: PathBuf },
    #[error("network access denied")]
    NetworkAccessDenied,
    #[error("stack depth limit exceeded: {depth}/{limit}")]
    StackDepthExceeded { depth: usize, limit: usize },
}

/// Sandboxed filesystem access wrapper
pub struct SandboxedFs {
    usage: Arc<std::sync::Mutex<ResourceUsage>>,
    limits: SandboxLimits,
}

impl SandboxedFs {
    pub fn new(limits: SandboxLimits) -> Self {
        Self {
            usage: Arc::new(std::sync::Mutex::new(ResourceUsage::new())),
            limits,
        }
    }

    pub fn read(&self, path: &Path) -> Result<Vec<u8>, SandboxError> {
        let usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        usage.check_path_access(path, &self.limits)?;
        usage.check_fs_read(&self.limits)?;
        drop(usage);

        std::fs::read(path).map_err(|_e| SandboxError::PathAccessDenied {
            path: path.to_path_buf(),
        })
    }

    pub fn write(&self, path: &Path, data: &[u8]) -> Result<(), SandboxError> {
        let usage = self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        usage.check_path_access(path, &self.limits)?;
        usage.check_fs_write(&self.limits)?;
        drop(usage);

        std::fs::write(path, data).map_err(|_e| SandboxError::PathAccessDenied {
            path: path.to_path_buf(),
        })
    }
}

// ─── Feature 40: Plugin dependency resolution ─────────────────────────

/// Pre-bundled dependency for WASM plugins
#[derive(Debug, Clone)]
pub struct BundledDependency {
    /// Package name (e.g., "lodash")
    pub name: String,
    /// Version string
    pub version: String,
    /// Bundled JS source code (ESM format)
    pub source: String,
    /// Exports map (export name → bundled path)
    pub exports: HashMap<String, String>,
}

/// Dependency resolver for WASM plugins
pub struct PluginDependencyResolver {
    /// Pre-bundled dependencies cache
    bundled: DashMap<String, BundledDependency>,
    /// Resolution cache: package@version → resolved source
    cache: DashMap<String, String>,
}

impl PluginDependencyResolver {
    pub fn new() -> Self {
        Self {
            bundled: DashMap::new(),
            cache: DashMap::new(),
        }
    }

    /// Pre-bundle a dependency for use in WASM plugins
    pub fn pre_bundle(&self, dep: BundledDependency) {
        let key = format!("{}@{}", dep.name, dep.version);
        self.cache.insert(key.clone(), dep.source.clone());
        self.bundled.insert(key, dep);
    }

    /// Resolve a dependency import within a WASM plugin
    pub fn resolve(&self, import_spec: &str) -> Option<String> {
        // Check cache first
        if let Some(source) = self.cache.get(import_spec) {
            return Some(source.clone());
        }

        // Try to find in bundled deps
        for entry in self.bundled.iter() {
            let dep = entry.value();
            if dep.name == import_spec || import_spec.starts_with(&format!("{}/", dep.name)) {
                return Some(dep.source.clone());
            }
        }

        None
    }

    /// Generate import map for a WASM plugin
    pub fn generate_import_map(&self) -> String {
        let mut imports = Vec::new();
        for entry in self.bundled.iter() {
            let dep = entry.value();
            imports.push(format!(r#"    "{}": "bundle:{}""#, dep.name, entry.key()));
        }
        format!("{{\n  \"imports\": {{\n{}\n  }}\n}}", imports.join(",\n"))
    }

    /// List all pre-bundled dependencies
    pub fn list_bundled(&self) -> Vec<(String, String)> {
        self.bundled
            .iter()
            .map(|e| (e.value().name.clone(), e.value().version.clone()))
            .collect()
    }
}

impl Default for PluginDependencyResolver {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Feature 41: Plugin lifecycle hooks ───────────────────────────────

/// Lifecycle hook types for plugins
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleHook {
    /// Called when file watching starts
    WatchStart,
    /// Called when a watched file changes
    WatchChange,
    /// Called when file watching stops
    WatchEnd,
    /// Called before transform
    BeforeTransform,
    /// Called after transform
    AfterTransform,
    /// Called before build
    BeforeBuild,
    /// Called after build
    AfterBuild,
    /// Called on dev server start
    DevServerStart,
    /// Called on dev server stop
    DevServerStop,
}

impl LifecycleHook {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WatchStart => "watchStart",
            Self::WatchChange => "watchChange",
            Self::WatchEnd => "watchEnd",
            Self::BeforeTransform => "beforeTransform",
            Self::AfterTransform => "afterTransform",
            Self::BeforeBuild => "beforeBuild",
            Self::AfterBuild => "afterBuild",
            Self::DevServerStart => "devServerStart",
            Self::DevServerStop => "devServerStop",
        }
    }
}

/// Context passed to lifecycle hooks
#[derive(Debug, Clone)]
pub struct HookContext {
    /// The hook type being invoked
    pub hook: LifecycleHook,
    /// File path that triggered the hook (if applicable)
    pub file_path: Option<String>,
    /// Additional metadata
    pub metadata: HashMap<String, String>,
}

impl HookContext {
    pub fn new(hook: LifecycleHook) -> Self {
        Self {
            hook,
            file_path: None,
            metadata: HashMap::new(),
        }
    }

    pub fn with_path(mut self, path: &str) -> Self {
        self.file_path = Some(path.to_string());
        self
    }

    pub fn with_metadata(mut self, key: &str, value: &str) -> Self {
        self.metadata.insert(key.to_string(), value.to_string());
        self
    }
}

/// Hook handler function type
pub type HookHandler = Arc<dyn Fn(&HookContext) + Send + Sync>;

/// Registry for plugin lifecycle hooks
pub struct LifecycleHookRegistry {
    hooks: DashMap<LifecycleHook, Vec<(String, HookHandler)>>,
}

impl LifecycleHookRegistry {
    pub fn new() -> Self {
        Self {
            hooks: DashMap::new(),
        }
    }

    /// Register a hook handler for a specific plugin
    pub fn register(
        &self,
        plugin_id: &str,
        hook: LifecycleHook,
        handler: impl Fn(&HookContext) + Send + Sync + 'static,
    ) {
        let mut handlers = self.hooks.entry(hook).or_default();
        handlers.push((plugin_id.to_string(), Arc::new(handler)));
    }

    /// Invoke all handlers for a hook
    pub fn invoke(&self, ctx: &HookContext) {
        if let Some(handlers) = self.hooks.get(&ctx.hook) {
            for (_plugin_id, handler) in handlers.iter() {
                handler(ctx);
            }
        }
    }

    /// Remove all hooks for a specific plugin
    pub fn unregister_plugin(&self, plugin_id: &str) {
        let hooks: Vec<LifecycleHook> = self.hooks.iter().map(|e| *e.key()).collect();
        for hook in hooks {
            if let Some(mut handlers) = self.hooks.get_mut(&hook) {
                handlers.retain(|(pid, _)| pid != plugin_id);
            }
        }
    }

    /// Get the count of registered hooks
    pub fn hook_count(&self, hook: LifecycleHook) -> usize {
        self.hooks.get(&hook).map(|h| h.len()).unwrap_or(0)
    }
}

impl Default for LifecycleHookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Feature 42: Plugin parallel execution ────────────────────────────

/// A plugin transform task that can be executed in parallel
pub struct PluginTransformTask {
    pub plugin_id: String,
    pub file_path: String,
    pub source: String,
    pub priority: u8, // 0 = highest
}

/// Result of a parallel plugin transform
pub struct PluginTransformResult {
    pub plugin_id: String,
    pub file_path: String,
    pub code: String,
    pub source_map: Option<String>,
    pub duration: Duration,
    pub error: Option<String>,
}

/// Execute plugin transforms in parallel using rayon
pub fn execute_parallel_transforms(
    tasks: Vec<PluginTransformTask>,
    transform_fn: impl Fn(&PluginTransformTask) -> Result<(String, Option<String>), String>
    + Send
    + Sync,
) -> Vec<PluginTransformResult> {
    use rayon::prelude::*;

    tasks
        .into_par_iter()
        .map(|task| {
            let start = Instant::now();
            let plugin_id = task.plugin_id.clone();
            let file_path = task.file_path.clone();

            match transform_fn(&task) {
                Ok((code, source_map)) => PluginTransformResult {
                    plugin_id,
                    file_path,
                    code,
                    source_map,
                    duration: start.elapsed(),
                    error: None,
                },
                Err(e) => PluginTransformResult {
                    plugin_id,
                    file_path,
                    code: String::new(),
                    source_map: None,
                    duration: start.elapsed(),
                    error: Some(e),
                },
            }
        })
        .collect()
}

/// Group tasks by independence — tasks that operate on different files can run in parallel
pub fn group_independent_tasks(tasks: Vec<PluginTransformTask>) -> Vec<Vec<PluginTransformTask>> {
    let mut groups: Vec<Vec<PluginTransformTask>> = Vec::new();

    for task in tasks {
        // Find a group where no task has the same file_path
        let group_idx = groups
            .iter()
            .position(|group| !group.iter().any(|t| t.file_path == task.file_path));

        match group_idx {
            Some(idx) => groups[idx].push(task),
            None => groups.push(vec![task]),
        }
    }

    groups
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hot_reloader_register() {
        let reloader = PluginHotReloader::new();
        let temp = std::env::temp_dir().join("test_plugin.js");
        std::fs::write(&temp, "module.exports = {};").unwrap();
        reloader.register("test-plugin", &temp);
        assert_eq!(reloader.plugin_sources.len(), 1);
    }

    #[test]
    fn test_hot_reloader_detect_change() {
        let reloader = PluginHotReloader::new();
        let temp = std::env::temp_dir().join("test_plugin_change.js");
        std::fs::write(&temp, "module.exports = {};").unwrap();
        reloader.register("test-plugin", &temp);

        // No changes yet
        let changed = reloader.check_for_changes();
        assert!(changed.is_empty());

        // Modify the file
        std::fs::write(&temp, "module.exports = { changed: true };").unwrap();
        let changed = reloader.check_for_changes();
        assert!(changed.contains(&"test-plugin".to_string()));
    }

    #[test]
    fn test_sandbox_limits_default() {
        let limits = SandboxLimits::default();
        assert_eq!(limits.max_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.max_cpu_time_ms, 5000);
        assert!(!limits.allow_network);
    }

    #[test]
    fn test_sandbox_memory_check() {
        let limits = SandboxLimits::default();
        let usage = ResourceUsage {
            memory_used: 100,
            cpu_time: Duration::ZERO,
            fs_reads: 0,
            fs_writes: 0,
            start_time: Instant::now(),
        };
        assert!(usage.check_memory(&limits).is_ok());

        let usage_over = ResourceUsage {
            memory_used: 128 * 1024 * 1024,
            cpu_time: Duration::ZERO,
            fs_reads: 0,
            fs_writes: 0,
            start_time: Instant::now(),
        };
        assert!(usage_over.check_memory(&limits).is_err());
    }

    #[test]
    fn test_sandbox_path_access() {
        let limits = SandboxLimits {
            allowed_paths: vec![PathBuf::from("./src")],
            ..Default::default()
        };
        let usage = ResourceUsage::new();
        assert!(
            usage
                .check_path_access(Path::new("./src/index.ts"), &limits)
                .is_ok()
        );
        assert!(
            usage
                .check_path_access(Path::new("/etc/passwd"), &limits)
                .is_err()
        );
    }

    #[test]
    fn test_dependency_resolver() {
        let resolver = PluginDependencyResolver::new();
        resolver.pre_bundle(BundledDependency {
            name: "lodash".to_string(),
            version: "4.17.21".to_string(),
            source: "export const _ = {};".to_string(),
            exports: HashMap::new(),
        });

        let resolved = resolver.resolve("lodash");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().contains("export const _"));
    }

    #[test]
    fn test_dependency_import_map() {
        let resolver = PluginDependencyResolver::new();
        resolver.pre_bundle(BundledDependency {
            name: "react".to_string(),
            version: "18.0.0".to_string(),
            source: "export const React = {};".to_string(),
            exports: HashMap::new(),
        });

        let map = resolver.generate_import_map();
        assert!(map.contains("react"));
        assert!(map.contains("bundle:"));
    }

    #[test]
    fn test_lifecycle_hooks() {
        let registry = LifecycleHookRegistry::new();
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();

        registry.register("test-plugin", LifecycleHook::WatchStart, move |_ctx| {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        assert_eq!(registry.hook_count(LifecycleHook::WatchStart), 1);

        let ctx = HookContext::new(LifecycleHook::WatchStart);
        registry.invoke(&ctx);

        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_lifecycle_hooks_unregister() {
        let registry = LifecycleHookRegistry::new();
        registry.register("plugin-a", LifecycleHook::BeforeBuild, |_| {});
        registry.register("plugin-b", LifecycleHook::BeforeBuild, |_| {});
        assert_eq!(registry.hook_count(LifecycleHook::BeforeBuild), 2);

        registry.unregister_plugin("plugin-a");
        assert_eq!(registry.hook_count(LifecycleHook::BeforeBuild), 1);
    }

    #[test]
    fn test_parallel_transforms() {
        let tasks = vec![
            PluginTransformTask {
                plugin_id: "p1".to_string(),
                file_path: "a.js".to_string(),
                source: "const a = 1;".to_string(),
                priority: 0,
            },
            PluginTransformTask {
                plugin_id: "p2".to_string(),
                file_path: "b.js".to_string(),
                source: "const b = 2;".to_string(),
                priority: 1,
            },
        ];

        let results = execute_parallel_transforms(tasks, |task| {
            Ok((
                format!("// transformed by {}\n{}", task.plugin_id, task.source),
                None,
            ))
        });

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.error.is_none()));
    }

    #[test]
    fn test_group_independent_tasks() {
        let tasks = vec![
            PluginTransformTask {
                plugin_id: "p1".to_string(),
                file_path: "a.js".to_string(),
                source: "".to_string(),
                priority: 0,
            },
            PluginTransformTask {
                plugin_id: "p2".to_string(),
                file_path: "a.js".to_string(),
                source: "".to_string(),
                priority: 0,
            },
            PluginTransformTask {
                plugin_id: "p3".to_string(),
                file_path: "b.js".to_string(),
                source: "".to_string(),
                priority: 0,
            },
        ];

        let groups = group_independent_tasks(tasks);
        // p1 and p3 can be in the same group (different files)
        // p2 must be in a separate group (same file as p1)
        assert!(groups.len() >= 2);
    }
}

// ─── G12.16: Error Messages with Source Context ────────────────────────

/// A rich error message with source context, caret pointer, and suggested fixes.
///
/// PledgePack errors include the source file, line number, surrounding context,
/// a caret pointing to the exact location, and suggested fixes — similar to
/// rustc's error messages.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SourceError {
    /// The error message.
    pub message: String,
    /// The error code (e.g., "PLEDGE-E001").
    pub code: String,
    /// Source file path.
    pub file: String,
    /// Line number (1-indexed).
    pub line: usize,
    /// Column number (1-indexed).
    pub column: usize,
    /// The source line content.
    pub source_line: String,
    /// Suggested fixes.
    pub fixes: Vec<SuggestedFix>,
    /// Related help text.
    pub help: Option<String>,
}

/// A suggested fix for an error.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SuggestedFix {
    /// Description of the fix.
    pub description: String,
    /// The replacement text.
    pub replacement: String,
    /// Start line of the fix range.
    pub start_line: usize,
    /// Start column of the fix range.
    pub start_col: usize,
    /// End line of the fix range.
    pub end_line: usize,
    /// End column of the fix range.
    pub end_col: usize,
}

impl SourceError {
    /// Create a new source error.
    pub fn new(code: &str, message: &str, file: &str, line: usize, column: usize) -> Self {
        Self {
            message: message.to_string(),
            code: code.to_string(),
            file: file.to_string(),
            line,
            column,
            source_line: String::new(),
            fixes: Vec::new(),
            help: None,
        }
    }

    /// Set the source line content.
    pub fn with_source_line(mut self, line: &str) -> Self {
        self.source_line = line.to_string();
        self
    }

    /// Add a suggested fix.
    pub fn with_fix(mut self, fix: SuggestedFix) -> Self {
        self.fixes.push(fix);
        self
    }

    /// Set help text.
    pub fn with_help(mut self, help: &str) -> Self {
        self.help = Some(help.to_string());
        self
    }

    /// Format the error with source context and caret pointer.
    ///
    /// Example output:
    /// ```text
    /// error[PLEDGE-E001]: Cannot resolve module "lodash"
    ///   --> src/main.ts:3:10
    ///    |
    ///  3 | import _ from "lodash";
    ///    |          ^^^^^^^^
    ///    |
    /// help: Install lodash with `npm install lodash`
    /// ```
    pub fn format(&self) -> String {
        let mut output = format!("error[{}]: {}\n", self.code, self.message);
        output.push_str(&format!(
            "  --> {}:{}:{}\n",
            self.file, self.line, self.column
        ));

        if !self.source_line.is_empty() {
            let line_num = format!("{}", self.line);
            let padding = " ".repeat(line_num.len());
            output.push_str(&format!("  {} |\n", padding));
            output.push_str(&format!(" {} | {}\n", line_num, self.source_line));

            // Draw caret pointer
            let caret = "^";
            let spaces = " ".repeat(self.column.saturating_sub(1));
            output.push_str(&format!("  {} | {}{}\n", padding, spaces, caret));
        }

        output.push_str("  |\n");

        if let Some(ref help) = self.help {
            output.push_str(&format!("help: {}\n", help));
        }

        for fix in &self.fixes {
            output.push_str(&format!("fix: {}\n", fix.description));
        }

        output
    }
}

// ─── G12.35: Plugin Signing Verification ───────────────────────────────

/// A plugin signature for verifying plugin integrity.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginSignature {
    /// The plugin name.
    pub plugin_name: String,
    /// The plugin version.
    pub version: String,
    /// The blake3 hash of the plugin WASM bytes.
    pub wasm_hash: String,
    /// The Ed25519 public key of the signer (hex-encoded).
    pub signer_public_key: String,
    /// The Ed25519 signature (hex-encoded).
    pub signature: String,
    /// The signer's identity (e.g., "@pledgelabs", "github:username").
    pub signer_identity: String,
    /// Unix timestamp when the signature was created.
    pub timestamp: u64,
    /// Whether the signature has been verified.
    pub verified: bool,
}

/// Constant-time equality for hashes / keys / signatures: the running time
/// does not depend on the position of the first differing byte (only on the
/// lengths, which are not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

/// Decode a hex-encoded 32-byte Ed25519 public key.
fn decode_public_key(hex_key: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex_key).ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

/// Plugin signing verifier.
pub struct PluginSigningVerifier {
    /// Known trusted public keys: (identity, public_key).
    trusted_keys: Vec<(String, String)>,
}

impl PluginSigningVerifier {
    /// Create a new verifier with no trusted keys.
    pub fn new() -> Self {
        Self {
            trusted_keys: Vec::new(),
        }
    }

    /// Add a trusted public key for an identity.
    pub fn trust_key(&mut self, identity: &str, public_key: &str) {
        self.trusted_keys
            .push((identity.to_string(), public_key.to_string()));
    }

    /// Verify a plugin signature.
    ///
    /// Checks that the signer is a trusted identity/key pair, then
    /// cryptographically verifies the Ed25519 signature over the plugin's
    /// blake3 WASM hash — mirroring the real (not stubbed) verification in
    /// `pledgepack_cache::advanced::verify_cache_entry`. Any failure to
    /// trust, decode, or verify returns `false`; nothing here panics on
    /// malformed input, since `sig` comes from a plugin manifest an attacker
    /// could control.
    pub fn verify(&self, sig: &PluginSignature) -> bool {
        // Decode the claimed key FIRST and compare raw bytes: comparing the
        // hex strings with `==` was both variable-time (early exit at the
        // first differing character) and case-sensitive (a trusted key
        // written in upper-case hex never matched its own signatures).
        let Some(claimed_key) = decode_public_key(&sig.signer_public_key) else {
            return false;
        };

        // Constant-time trust check: every trusted entry is compared and the
        // results are accumulated, so timing reveals neither which entry
        // matched nor how many leading bytes agreed.
        let mut trusted = Choice::from(0);
        for (identity, key) in &self.trusted_keys {
            let identity_ok = identity.as_bytes().ct_eq(sig.signer_identity.as_bytes());
            let key_ok = match decode_public_key(key) {
                Some(k) => k[..].ct_eq(&claimed_key[..]),
                None => Choice::from(0),
            };
            trusted |= identity_ok & key_ok;
        }
        if !bool::from(trusted) {
            return false;
        }

        let Ok(verifying_key) = VerifyingKey::from_bytes(&claimed_key) else {
            return false;
        };

        let Ok(sig_bytes) = hex::decode(&sig.signature) else {
            return false;
        };
        let Ok(signature) = Signature::from_slice(&sig_bytes) else {
            return false;
        };

        // `verify_strict` (not `verify`): also rejects small-order public
        // keys and non-canonical signatures, closing the signature
        // malleability / "weak key verifies anything" cases of plain verify.
        verifying_key
            .verify_strict(sig.wasm_hash.as_bytes(), &signature)
            .is_ok()
    }

    /// Verify and return a signed plugin signature.
    pub fn verify_signature(&self, mut sig: PluginSignature) -> PluginSignature {
        sig.verified = self.verify(&sig);
        sig
    }

    /// Number of trusted keys.
    pub fn trusted_key_count(&self) -> usize {
        self.trusted_keys.len()
    }
}

impl Default for PluginSigningVerifier {
    fn default() -> Self {
        Self::new()
    }
}

// ─── G12.36: Plugin Capability Audit ───────────────────────────────────

/// The capabilities a plugin can request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PluginCapability {
    /// Read files from the file system.
    FileSystemRead,
    /// Write files to the file system.
    FileSystemWrite,
    /// Make network requests.
    Network,
    /// Access environment variables.
    Environment,
    /// Spawn child processes.
    ProcessSpawn,
    /// Access the cache store.
    CacheAccess,
    /// Read from stdin.
    StdinRead,
    /// Write to stdout/stderr.
    StdoutWrite,
    /// Custom capability (e.g., "database:read").
    Custom(String),
}

impl PluginCapability {
    /// Get the string representation.
    pub fn as_str(&self) -> &str {
        match self {
            PluginCapability::FileSystemRead => "fs:read",
            PluginCapability::FileSystemWrite => "fs:write",
            PluginCapability::Network => "network",
            PluginCapability::Environment => "env",
            PluginCapability::ProcessSpawn => "process:spawn",
            PluginCapability::CacheAccess => "cache",
            PluginCapability::StdinRead => "stdin:read",
            PluginCapability::StdoutWrite => "stdout:write",
            PluginCapability::Custom(s) => s.as_str(),
        }
    }

    /// Whether this is a high-risk capability.
    pub fn is_high_risk(&self) -> bool {
        matches!(
            self,
            PluginCapability::FileSystemWrite
                | PluginCapability::Network
                | PluginCapability::ProcessSpawn
        )
    }

    /// Parse a capability from its string form (as used in `sig.json`
    /// sidecars and `pluginSecurity` config). Unknown strings become
    /// `Custom`, matching how sidecars declare non-standard capabilities.
    pub fn parse(s: &str) -> Self {
        match s {
            "fs:read" => PluginCapability::FileSystemRead,
            "fs:write" => PluginCapability::FileSystemWrite,
            "network" => PluginCapability::Network,
            "env" => PluginCapability::Environment,
            "process:spawn" => PluginCapability::ProcessSpawn,
            "cache" => PluginCapability::CacheAccess,
            "stdin:read" => PluginCapability::StdinRead,
            "stdout:write" => PluginCapability::StdoutWrite,
            other => PluginCapability::Custom(other.to_string()),
        }
    }
}

/// Signature sidecar (`<plugin>.sig.json`) shared by the JS and WASM plugin
/// hosts. Flattens the signature fields with an optional declared-capability
/// list — a sidecar written before capabilities existed (bare
/// `PluginSignature`) still parses, with `capabilities` defaulting to empty.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PluginTrustSidecar {
    /// The Ed25519 signature + signer metadata.
    #[serde(flatten)]
    pub signature: PluginSignature,
    /// Capabilities the plugin declares it needs; audited against policy
    /// when a `CapabilityAuditor` is configured.
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
}

/// A capability audit for a plugin.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CapabilityAudit {
    /// The plugin name.
    pub plugin_name: String,
    /// The plugin version.
    pub version: String,
    /// Requested capabilities.
    pub capabilities: Vec<PluginCapability>,
    /// Whether the audit passed (all capabilities are approved).
    pub approved: bool,
    /// Any denied capabilities.
    pub denied: Vec<PluginCapability>,
    /// Audit timestamp.
    pub timestamp: u64,
    /// Audit notes.
    pub notes: String,
}

/// Capability auditor that checks plugin capabilities against a policy.
pub struct CapabilityAuditor {
    /// Approved capabilities (capabilities that are always allowed).
    approved: Vec<PluginCapability>,
    /// Denied capabilities (capabilities that are always blocked).
    denied: Vec<PluginCapability>,
}

impl CapabilityAuditor {
    /// Create a new auditor with default policy.
    pub fn new() -> Self {
        Self {
            approved: vec![
                PluginCapability::FileSystemRead,
                PluginCapability::CacheAccess,
                PluginCapability::StdoutWrite,
            ],
            denied: vec![PluginCapability::ProcessSpawn],
        }
    }

    /// Approve an additional capability.
    pub fn approve(&mut self, cap: PluginCapability) {
        self.approved.push(cap);
    }

    /// Deny a capability.
    pub fn deny(&mut self, cap: PluginCapability) {
        self.denied.push(cap);
    }

    /// Audit a plugin's requested capabilities.
    pub fn audit(
        &self,
        plugin_name: &str,
        version: &str,
        caps: Vec<PluginCapability>,
    ) -> CapabilityAudit {
        let mut denied_caps = Vec::new();
        for cap in &caps {
            if self.denied.contains(cap) {
                denied_caps.push(cap.clone());
            }
        }

        let approved = denied_caps.is_empty();
        let notes = if denied_caps.is_empty() {
            "All capabilities approved".to_string()
        } else {
            format!(
                "Denied capabilities: {}",
                denied_caps
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        CapabilityAudit {
            plugin_name: plugin_name.to_string(),
            version: version.to_string(),
            capabilities: caps,
            approved,
            denied: denied_caps,
            timestamp: 0,
            notes,
        }
    }

    /// Get all high-risk capabilities from an audit.
    pub fn high_risk_capabilities(audit: &CapabilityAudit) -> Vec<&PluginCapability> {
        audit
            .capabilities
            .iter()
            .filter(|c| c.is_high_risk())
            .collect()
    }
}

impl Default for CapabilityAuditor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod g12_tests {
    use super::*;

    #[test]
    fn g12_16_source_error_format() {
        let error = SourceError::new(
            "PLEDGE-E001",
            "Cannot resolve module \"lodash\"",
            "src/main.ts",
            3,
            10,
        )
        .with_source_line("import _ from \"lodash\";")
        .with_help("Install lodash with `npm install lodash`");

        let formatted = error.format();
        assert!(formatted.contains("error[PLEDGE-E001]"));
        assert!(formatted.contains("src/main.ts:3:10"));
        assert!(formatted.contains("import _ from"));
        assert!(formatted.contains("help: Install lodash"));
    }

    #[test]
    fn g12_16_source_error_with_fix() {
        let fix = SuggestedFix {
            description: "Replace \"lodash\" with \"lodash-es\"".to_string(),
            replacement: "lodash-es".to_string(),
            start_line: 3,
            start_col: 10,
            end_line: 3,
            end_col: 18,
        };
        let error =
            SourceError::new("PLEDGE-E002", "Module not found", "src/app.ts", 3, 10).with_fix(fix);

        assert_eq!(error.fixes.len(), 1);
        assert!(error.fixes[0].description.contains("lodash-es"));
    }

    /// Deterministic ed25519 keypair for tests — not for production use.
    fn test_keypair() -> (ed25519_dalek::SigningKey, ed25519_dalek::VerifyingKey) {
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();
        (signing_key, verifying_key)
    }

    #[test]
    fn g12_35_plugin_signing_verifier() {
        use ed25519_dalek::Signer;
        let (signing_key, verifying_key) = test_keypair();
        let wasm_hash = "hash123";
        let signature = signing_key.sign(wasm_hash.as_bytes());

        let mut verifier = PluginSigningVerifier::new();
        verifier.trust_key("@pledgelabs", &hex::encode(verifying_key.to_bytes()));

        let sig = PluginSignature {
            plugin_name: "@pledge/css".to_string(),
            version: "1.0.0".to_string(),
            wasm_hash: wasm_hash.to_string(),
            signer_public_key: hex::encode(verifying_key.to_bytes()),
            signature: hex::encode(signature.to_bytes()),
            signer_identity: "@pledgelabs".to_string(),
            timestamp: 1234567890,
            verified: false,
        };

        assert!(verifier.verify(&sig));
        assert_eq!(verifier.trusted_key_count(), 1);
    }

    #[test]
    fn signing_trust_check_is_case_insensitive_over_decoded_key_bytes() {
        use ed25519_dalek::Signer;
        let (signing_key, verifying_key) = test_keypair();
        let signature = signing_key.sign(b"hash123");

        // Trusted key configured in UPPER-case hex; the sidecar carries the
        // same key in lower-case. The bytes are identical, so this must trust.
        let mut verifier = PluginSigningVerifier::new();
        verifier.trust_key(
            "@pledgelabs",
            &hex::encode(verifying_key.to_bytes()).to_uppercase(),
        );
        let sig = PluginSignature {
            plugin_name: "p".into(),
            version: "1.0.0".into(),
            wasm_hash: "hash123".into(),
            signer_public_key: hex::encode(verifying_key.to_bytes()),
            signature: hex::encode(signature.to_bytes()),
            signer_identity: "@pledgelabs".into(),
            timestamp: 0,
            verified: false,
        };
        assert!(verifier.verify(&sig));

        // Same key, wrong identity: not trusted.
        let mut wrong_identity = sig.clone();
        wrong_identity.signer_identity = "@someone-else".into();
        assert!(!verifier.verify(&wrong_identity));

        // A different (untrusted) key of the same length: not trusted.
        let other = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let mut other_key = sig.clone();
        other_key.signer_public_key = hex::encode(other.verifying_key().to_bytes());
        other_key.signature = hex::encode(other.sign(b"hash123").to_bytes());
        assert!(!verifier.verify(&other_key));
    }

    #[test]
    fn signing_rejects_small_order_key_that_plain_verify_accepts() {
        // The identity point (small order) with an all-identity signature
        // R = identity, s = 0 satisfies the *cofactorless* check for every
        // message, so plain `verify` accepts a "signature" nobody produced.
        // `verify_strict` must reject it even when the key is trusted.
        let mut identity_point = [0u8; 32];
        identity_point[0] = 1;
        let mut forged = [0u8; 64];
        forged[0] = 1; // R = identity, s = 0

        let mut verifier = PluginSigningVerifier::new();
        verifier.trust_key("@weak", &hex::encode(identity_point));
        let sig = PluginSignature {
            plugin_name: "p".into(),
            version: "1.0.0".into(),
            wasm_hash: "anything".into(),
            signer_public_key: hex::encode(identity_point),
            signature: hex::encode(forged),
            signer_identity: "@weak".into(),
            timestamp: 0,
            verified: false,
        };
        assert!(!verifier.verify(&sig));
    }

    #[test]
    fn ct_eq_matches_slice_equality() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn g12_35_plugin_signing_rejects_untrusted() {
        let verifier = PluginSigningVerifier::new();
        let sig = PluginSignature {
            plugin_name: "evil-plugin".to_string(),
            version: "1.0.0".to_string(),
            wasm_hash: "hash123".to_string(),
            signer_public_key: "unknownkey".to_string(),
            signature: "sig456".to_string(),
            signer_identity: "@unknown".to_string(),
            timestamp: 0,
            verified: false,
        };

        assert!(!verifier.verify(&sig));
    }

    #[test]
    fn g12_35_plugin_signing_rejects_forged_signature_from_trusted_identity() {
        // Regression test for the original bug (PRODUCTION-READINESS-100.md
        // goal 11): `verify()` used to accept ANY non-empty signature string
        // as long as the identity+pubkey pair was trusted, without checking
        // the signature bytes cryptographically at all. A malicious or
        // corrupted plugin claiming a trusted identity+pubkey must still
        // fail verification if its signature doesn't actually check out.
        let (_signing_key, verifying_key) = test_keypair();

        let mut verifier = PluginSigningVerifier::new();
        verifier.trust_key("@pledgelabs", &hex::encode(verifying_key.to_bytes()));

        let sig = PluginSignature {
            plugin_name: "evil-plugin".to_string(),
            version: "1.0.0".to_string(),
            wasm_hash: "hash123".to_string(),
            signer_public_key: hex::encode(verifying_key.to_bytes()),
            // Well-formed hex of the right length, but not a real signature
            // over "hash123" produced by this key.
            signature: hex::encode([0u8; 64]),
            signer_identity: "@pledgelabs".to_string(),
            timestamp: 0,
            verified: false,
        };

        assert!(!verifier.verify(&sig));
    }

    #[test]
    fn g12_35_plugin_signing_rejects_malformed_hex() {
        // The old code's fallback was "non-empty string" — garbage hex must
        // now fail cleanly (no panic) rather than accidentally pass.
        let (_signing_key, verifying_key) = test_keypair();
        let mut verifier = PluginSigningVerifier::new();
        verifier.trust_key("@pledgelabs", &hex::encode(verifying_key.to_bytes()));

        let sig = PluginSignature {
            plugin_name: "@pledge/css".to_string(),
            version: "1.0.0".to_string(),
            wasm_hash: "hash123".to_string(),
            signer_public_key: hex::encode(verifying_key.to_bytes()),
            signature: "not-valid-hex!!".to_string(),
            signer_identity: "@pledgelabs".to_string(),
            timestamp: 0,
            verified: false,
        };

        assert!(!verifier.verify(&sig));
    }

    #[test]
    fn g12_36_capability_audit_approved() {
        let auditor = CapabilityAuditor::new();
        let audit = auditor.audit(
            "@pledge/css",
            "1.0.0",
            vec![
                PluginCapability::FileSystemRead,
                PluginCapability::CacheAccess,
            ],
        );

        assert!(audit.approved);
        assert!(audit.denied.is_empty());
    }

    #[test]
    fn g12_36_capability_audit_denied() {
        let auditor = CapabilityAuditor::new();
        let audit = auditor.audit(
            "suspicious-plugin",
            "1.0.0",
            vec![
                PluginCapability::FileSystemRead,
                PluginCapability::ProcessSpawn,
            ],
        );

        assert!(!audit.approved);
        assert_eq!(audit.denied.len(), 1);
        assert_eq!(audit.denied[0], PluginCapability::ProcessSpawn);
    }

    #[test]
    fn g12_36_capability_high_risk() {
        assert!(PluginCapability::Network.is_high_risk());
        assert!(PluginCapability::FileSystemWrite.is_high_risk());
        assert!(PluginCapability::ProcessSpawn.is_high_risk());
        assert!(!PluginCapability::FileSystemRead.is_high_risk());
        assert!(!PluginCapability::CacheAccess.is_high_risk());
    }

    #[test]
    fn g12_36_capability_audit_custom() {
        let auditor = CapabilityAuditor::new();
        let audit = auditor.audit(
            "custom-plugin",
            "1.0.0",
            vec![PluginCapability::Custom("database:read".to_string())],
        );

        // Custom capabilities are not in the denied list, so they're approved
        assert!(audit.approved);
    }
}
