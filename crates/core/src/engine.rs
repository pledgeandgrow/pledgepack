// Build engine: orchestrates the entire build pipeline
//
// This is the "Turbo engine" equivalent — a function-level
// incremental computation system that caches aggressively
// and only recomputes what changed.

use crate::config::PledgeConfig;
use crate::module::{ModuleId, ModuleKind, ResolvedModule};
use crate::module_graph::SerializableModuleGraph;
use crate::plugin_hooks::{HookRunner, PluginHooks};
use anyhow::{Result, bail};
use dashmap::DashMap;
use pledgepack_native_sys::Graph;
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Read a file as a string, using memory-mapped I/O for large files (>64KB).
/// Falls back to standard `std::fs::read_to_string` for smaller files where
/// mmap setup overhead outweighs the zero-copy benefit.
fn read_file_mmap(path: &std::path::Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > 65536 {
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(String::from_utf8_lossy(mmap.as_ref()).into_owned())
    } else {
        Ok(std::fs::read_to_string(path)?)
    }
}

/// Remove `<script ... src="{entry}">...</script>` tags from an HTML
/// template for each of `entries` (matched with or without a leading `/`,
/// since `entries` come from `config.entry` — typically written without one,
/// e.g. `"src/index.tsx"` — while the template's own markup conventionally
/// uses an absolute path, e.g. `src="/src/index.tsx"`).
///
/// Production HTML generation (see `emit()`) inserts a new `<script>` tag
/// pointing at the built, hashed chunk for each entry; without this, the
/// template's original dev-mode entry tag (loading the raw, untransformed
/// source file) is left in place alongside it, producing a page that 404s
/// trying to load a source path the production output never serves.
fn remove_entry_script_tags(html: &str, entries: &[String]) -> String {
    let mut html = html.to_string();
    for entry in entries {
        let trimmed = entry.trim_start_matches("./").trim_start_matches('/');
        // Matches a self-closed-content `<script ...src="{entry}"...></script>`
        // tag regardless of attribute order or whether the leading `/` is
        // present in the markup.
        let pattern = format!(
            r#"(?s)<script\b[^>]*\bsrc\s*=\s*["']/?{}["'][^>]*>\s*</script>\s*\n?"#,
            regex::escape(trimmed)
        );
        if let Ok(re) = Regex::new(&pattern) {
            html = re.replace_all(&html, "").into_owned();
        }
    }
    html
}

/// Maximum number of modules the engine will hold before bailing.
/// Protects against unbounded memory growth on pathological builds.
const MAX_MODULES: usize = 50_000;

/// Module count at which we start warning about memory usage.
const WARN_MODULES: usize = 10_000;

// PRODUCTION-READINESS-100.md goal 91: `read_file_bytes_mmap` was removed
// from here — zero call sites; `module_graph.rs` and `asset_pipeline.rs`
// each have their own mmap-based readers already wired into real call paths.

/// File-based lock guarding the output directory against concurrent builds.
///
/// Two `pledge build` processes writing to the same `out_dir` can corrupt
/// each other's output (both wipe and recreate the directory). The lock
/// file lives NEXT TO the output directory — not inside it — because emit()
/// deletes and recreates `out_dir`, which would remove a lock placed within.
///
/// Acquisition is atomic via `create_new`, a lock older than
/// [`OutputLock::STALE_AFTER`] is treated as abandoned (crashed build) and
/// reclaimed, and the file is removed when the guard drops.
struct OutputLock {
    path: PathBuf,
}

impl OutputLock {
    /// How old a lock must be before it is considered stale. A live build
    /// finishes well within this window; a lock older than it was left
    /// behind by a crashed or killed process.
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(300);

    /// Acquire the output lock for `out_dir`, or bail if another build
    /// holds a fresh lock.
    fn acquire(out_dir: &Path) -> Result<Self> {
        let dir_name = out_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "out".to_string());
        let lock_path = out_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(".{}.pledge.lock", dir_name));

        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = writeln!(file, "pid {}", std::process::id());
                    return Ok(Self { path: lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // A lock whose mtime is in the future (clock skew) is
                    // treated as fresh — safer to bail than to corrupt a
                    // concurrent build's output.
                    let is_stale = std::fs::metadata(&lock_path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age >= Self::STALE_AFTER);
                    if is_stale && attempt == 0 {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }
                    bail!(
                        "Another build process appears to be running (lock file {}). \
                         Delete it to force.",
                        crate::display_path(&lock_path)
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        bail!(
            "Could not acquire the output lock {} after retrying",
            crate::display_path(&lock_path)
        )
    }
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Manifest entry for build manifest.json with entry-to-chunk mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Hashed output filename (e.g., "index.a1b2c3d4.js")
    pub file: String,
    /// Whether this is an entry point
    pub is_entry: bool,
    /// Whether this is a CSS file
    pub is_css: bool,
    /// Whether this is an async chunk (dynamic import)
    pub is_async: bool,
    /// Dynamic imports this module depends on
    pub imports: Vec<String>,
    /// CSS file associated with this module (if any)
    pub css: Option<String>,
}

/// A chunk of modules produced by the optimizer that should be emitted as a
/// single output file. This mirrors `pledgepack_optimizer::Chunk` but lives in
/// core to avoid a cyclic dependency (optimizer depends on core, not vice
/// versa). The CLI converts optimizer chunks into this representation before
/// passing them to `BuildEngine::emit_with_chunks`.
#[derive(Debug, Clone)]
pub struct EmitChunk {
    /// Chunk identifier (e.g., "entry-0", "vendor", "shared")
    pub id: String,
    /// Module IDs that belong to this chunk
    pub modules: Vec<ModuleId>,
    /// Whether this is an entry chunk (vs. vendor/shared/async)
    pub is_entry: bool,
}

pub struct BuildEngine {
    config: Arc<PledgeConfig>,
    /// Zig-backed dependency graph. Wrapped in `Mutex` because `Graph` is
    /// `Send` but not `Sync` — the mutex makes `BuildEngine` `Sync`.
    graph: Mutex<Graph>,
    /// Map from file path to module ID
    path_to_id: HashMap<PathBuf, ModuleId>,
    /// Cached resolved modules
    modules: HashMap<ModuleId, ResolvedModule>,
    /// Function-level cache (content hash → cached output).
    /// `DashMap` provides interior mutability so readers don't need `&mut self`
    /// and the map is safe to share across threads (`Sync`).
    function_cache: DashMap<u64, CachedOutput>,
    /// Persistent function-level cache (disk-backed)
    persistent_cache: Option<pledgepack_cache::FunctionCache>,
    /// Results of the plugin `transform` chain, keyed by
    /// `blake3(plugin-chain fingerprint, module id, input source)`. `None`
    /// records "no plugin changed this module" so it is not re-run either.
    /// Only populated for chains whose plugins all report a
    /// [`PluginHooks::transform_fingerprint`].
    plugin_transform_cache: HashMap<[u8; 32], Option<crate::plugin_hooks::CodeResult>>,
    /// Source map of the plugin transform chain per module
    /// (`post-plugin source -> original`). Composed with the built-in
    /// transform's map at emit time so the final map points at the file on
    /// disk instead of the intermediate plugin output.
    plugin_source_maps: HashMap<ModuleId, String>,
    /// Remote cache for sharing across machines
    remote_cache: Option<pledgepack_cache::remote::RemoteCache>,
    /// Serializable module graph for incremental rebuilds
    module_graph: SerializableModuleGraph,
    /// Previous module graph loaded from disk (for incremental comparison)
    previous_graph: Option<SerializableModuleGraph>,
    /// Git-based cache invalidator
    git_invalidator: Option<pledgepack_cache::git_cache::GitCacheInvalidator>,
    /// Whether this is an incremental rebuild (not first build)
    is_incremental: bool,
    /// Auto-discovered entry points from appDir (populated by build())
    auto_entries: Vec<String>,
    /// Module IDs of the actual entry points (not all modules)
    entry_module_ids: Vec<ModuleId>,
    /// Accumulated i18n translation catalog from all modules (#13)
    i18n_catalog: crate::i18n::TranslationCatalog,
    /// Task-graph engine driving the read/parse/transform pipeline.
    /// `None` when `PLEDGE_LEGACY_ENGINE` is set — the legacy
    /// function_cache/persistent/remote path is used instead.
    task_engine: Option<Arc<crate::task_transform::TaskTransformEngine>>,
}

#[derive(Debug, Clone)]
pub struct CachedOutput {
    pub code: String,
    pub source_map: Option<String>,
    pub deps: Vec<String>,
    pub is_css: bool,
    pub css_modules: Option<Vec<(String, String)>>,
    pub extracted_css: Option<String>,
    pub is_worker: bool,
    pub dynamic_imports: Vec<String>,
}

#[derive(Debug)]
pub struct BuildResult {
    pub modules_built: usize,
    pub modules_cached: usize,
    pub duration_ms: u128,
}

/// Build the rayon pool used for parallel module transforms, degrading to
/// fewer threads if the requested size can't be created (e.g. under thread
/// limits) and returning an error — rather than panicking — if even a
/// single-threaded pool cannot be built.
fn build_transform_pool(parallelism: usize) -> Result<rayon::ThreadPool> {
    for threads in [parallelism, 4, 1] {
        if let Ok(pool) = rayon::ThreadPoolBuilder::new().num_threads(threads).build() {
            return Ok(pool);
        }
    }
    bail!("failed to create a rayon thread pool for module transforms")
}

impl BuildEngine {
    pub fn new(config: Arc<PledgeConfig>) -> Self {
        let persistent_cache = if config.cache.enabled {
            Some(pledgepack_cache::FunctionCache::new(
                config.cache.dir.clone(),
                true,
            ))
        } else {
            None
        };

        // Initialize remote cache if configured
        let remote_cache = if config.cache.enabled && config.cache.remote.enabled {
            let remote_config = pledgepack_cache::remote::RemoteCacheConfig {
                backend: config.cache.remote.backend.clone(),
                endpoint: config.cache.remote.endpoint.clone(),
                bucket: config.cache.remote.bucket.clone(),
                region: config.cache.remote.region.clone(),
                access_key: None,
                secret_key: None,
                namespace: config.cache.remote.namespace.clone(),
                timeout_secs: 30,
                enabled: true,
            };
            let rc = pledgepack_cache::remote::RemoteCache::new(remote_config);
            if rc.is_enabled() { Some(rc) } else { None }
        } else {
            None
        };

        // Initialize git-based invalidator
        let git_invalidator = pledgepack_cache::git_cache::GitCacheInvalidator::new(&config.root);

        // Try to load previous module graph from disk for incremental rebuilds
        let graph_path = config.cache.dir.join("module_graph.bin");
        let previous_graph =
            if config.cache.enabled && SerializableModuleGraph::exists_on_disk(&graph_path) {
                match SerializableModuleGraph::load_from_disk(&graph_path) {
                    Ok(g) => {
                        info!("Loaded previous module graph: {} modules", g.modules.len());
                        Some(g)
                    }
                    Err(e) => {
                        warn!("Failed to load previous module graph: {}", e);
                        None
                    }
                }
            } else {
                None
            };

        let is_incremental = previous_graph.is_some();

        // Task-graph engine: the default pipeline. `PLEDGE_LEGACY_ENGINE`
        // opts back into the manual function_cache/persistent/remote path.
        let task_engine = if std::env::var_os("PLEDGE_LEGACY_ENGINE").is_some() {
            None
        } else {
            let mut builder = pledgepack_task_system::TaskEngineBuilder::new(
                pledgepack_task_system::TaskRegistry::new(),
            );
            if config.cache.enabled
                && let Ok(cas) = pledgepack_task_system::CasBackend::new(config.cache.dir.clone())
            {
                builder = builder.with_cas(cas);
            }
            if let Some(ref rc) = remote_cache {
                builder = builder.with_remote(rc.clone());
            }
            Some(Arc::new(
                crate::task_transform::TaskTransformEngine::from_engine(builder.build()),
            ))
        };

        // Initialize the stack canary with a random value before any Zig
        // native code (which uses stack protection) runs.
        pledgepack_native_sys::init_stack_canary();

        Self {
            config,
            graph: Mutex::new(Graph::new()),
            path_to_id: HashMap::new(),
            modules: HashMap::new(),
            function_cache: DashMap::new(),
            persistent_cache,
            plugin_transform_cache: HashMap::new(),
            plugin_source_maps: HashMap::new(),
            remote_cache,
            module_graph: SerializableModuleGraph::new(),
            previous_graph,
            git_invalidator: Some(git_invalidator).filter(|g| g.is_available()),
            is_incremental,
            auto_entries: Vec::new(),
            entry_module_ids: Vec::new(),
            i18n_catalog: crate::i18n::TranslationCatalog::default(),
            task_engine,
        }
    }

    /// Run a full build (dev or production)
    ///
    /// Supports incremental rebuilds: if a previous module graph was loaded
    /// from disk, only changed modules and their dependents are re-transformed.
    /// Unchanged modules are loaded from cache (memory → disk → remote).
    pub async fn build(&mut self) -> Result<BuildResult> {
        self.build_with_hooks(None)
    }

    /// [`build`](Self::build) with per-module plugin hooks.
    ///
    /// * `resolveId` runs before the built-in resolver for every import and
    ///   entry (first non-null plugin result wins; `external` results are left
    ///   unbundled).
    /// * `load` runs before the file is read (first non-null wins); it is what
    ///   provides the code of virtual modules.
    /// * `transform` hooks are chained in plugin order over the loaded source,
    ///   *before* the built-in TS/JSX/CSS transform — so the module graph, the
    ///   content hash and every cache are keyed on the post-plugin source.
    ///   (`enforce: "post"` placement after the built-in transform is not
    ///   supported by the production pipeline.)
    ///
    /// Hooks are invoked sequentially from the calling thread (plugin
    /// runtimes are not `Send`), never from the rayon transform pool. The
    /// `renderChunk` / `transformIndexHtml` hooks run at emit time — see
    /// [`emit_with_chunks_hooks`](Self::emit_with_chunks_hooks). A plugin
    /// error aborts the build with the plugin name and file in the message.
    ///
    /// This is a synchronous function: the only reason [`build`](Self::build)
    /// is `async` is API compatibility; nothing in the pipeline awaits.
    pub fn build_with_hooks(&mut self, hooks: Option<&dyn PluginHooks>) -> Result<BuildResult> {
        let runner = hooks.map(HookRunner::new);
        let runner = runner.as_ref();
        let start = std::time::Instant::now();

        // Phase 0: Auto-discover entry points from appDir if no explicit entry configured
        let mut auto_entries: Vec<String> = Vec::new();
        if self.config.entry.is_empty() {
            if let Some(app_dir) = self.config.resolve_app_dir() {
                let app_path = self.config.root.join(&app_dir);
                if app_path.is_dir() {
                    // Generate virtual entry + router modules for production build
                    let gen_dir = self.config.root.join(".pledge").join("gen");
                    std::fs::create_dir_all(&gen_dir)?;

                    // Scan app directory for routes
                    let route_table = crate::router::scan_app_dir(&self.config.root, &app_dir)?;

                    if route_table.routes.is_empty() {
                        anyhow::bail!(
                            "No pages found in {}/. Create {} to get started.",
                            app_dir,
                            app_dir.trim_end_matches('/').to_string() + "/page.tsx"
                        );
                    }
                    // Use relative paths for build (gen dir is .pledge/gen/, so ../../ to reach root)
                    let router_code = route_table.generate_router_module_build("../../");
                    let router_path = gen_dir.join("__pledge_router.tsx");
                    std::fs::write(&router_path, &router_code)?;

                    // Generate entry module (same as dev server's generate_entry_module)
                    let entry_code = r#"// Auto-generated by Pledge build — do not edit
// Entry module with route rendering and SPA navigation.

import React from "react";
import { createRoot } from "react-dom/client";
import { render } from "./__pledge_router";

var root = createRoot(document.getElementById("root"));

function renderApp() {
  var pathname = window.location.pathname;
  var element = render(pathname);
  root.render(element);
}

renderApp();

window.addEventListener("popstate", renderApp);

document.addEventListener("click", function(e) {
  var target = e.target;
  var anchor = target.closest && target.closest("a");
  if (anchor && anchor.href.startsWith(window.location.origin) && !anchor.target) {
    e.preventDefault();
    var url = new URL(anchor.href);
    window.history.pushState({}, "", url.pathname);
    renderApp();
  }
});
"#;
                    let entry_path = gen_dir.join("__pledge_entry.tsx");
                    std::fs::write(&entry_path, entry_code)?;

                    // Use the virtual entry as the build entry point
                    let entry_str = crate::normalize_path(&entry_path);
                    auto_entries.push(entry_str);

                    tracing::info!(
                        "Auto-discovered entry from app/ directory: {} routes",
                        route_table.routes.len()
                    );
                }
            }
        } else {
            // Entry points exist (e.g. from HTML), but still generate __pledge_router
            // so that entry.tsx's `import { render } from "/__pledge_router"` can resolve
            if let Some(app_dir) = self.config.resolve_app_dir() {
                let app_path = self.config.root.join(&app_dir);
                if app_path.is_dir() {
                    let gen_dir = self.config.root.join(".pledge").join("gen");
                    std::fs::create_dir_all(&gen_dir)?;

                    let route_table = crate::router::scan_app_dir(&self.config.root, &app_dir)?;
                    if !route_table.routes.is_empty() {
                        let router_code = route_table.generate_router_module_build("../../");
                        let router_path = gen_dir.join("__pledge_router.tsx");
                        std::fs::write(&router_path, &router_code)?;
                        tracing::info!(
                            "Generated __pledge_router for build: {} routes",
                            route_table.routes.len()
                        );
                    }
                }
            }
        }

        // Store auto-discovered entries for emit() to use
        if !auto_entries.is_empty() {
            self.auto_entries = auto_entries.clone();
        }

        // Phase 1: Resolve entry points (lazy — only resolve entries first)
        let entries: Vec<String> = if !self.auto_entries.is_empty() {
            self.auto_entries.clone()
        } else {
            self.config.entry.clone()
        };

        if entries.is_empty() {
            anyhow::bail!(
                "No pages found. Create app/page.tsx to get started, \n\
                 or set `entry` in pledge.config.ts if you're using a custom setup."
            );
        }

        for entry in &entries {
            if let Err(e) = self.resolve_and_add(entry, None, runner) {
                let err_str = e.to_string();
                if err_str.contains("Cannot resolve module") {
                    let entry_path = self.config.root.join(entry);
                    if !entry_path.exists() {
                        anyhow::bail!(
                            "File not found: {} \n\
                             This file is referenced in your pledge.config.ts. \n\
                             Create it or fix the path.",
                            entry
                        );
                    }
                }
                return Err(e);
            }
        }

        // Record entry module IDs in the serializable graph
        let entry_ids: Vec<ModuleId> = self.path_to_id.values().copied().collect();
        self.module_graph.set_entries(entry_ids.clone());
        // Store actual entry module IDs for optimizer use
        self.entry_module_ids = entry_ids.clone();

        // The task-graph path subsumes the incremental machinery: TaskIds
        // are content-addressed, so unchanged modules hit the task cache
        // (memory → disk → remote) without the previous-graph comparison.
        let task_driven = self.task_engine.is_some();

        // Phase 2: legacy incremental preload (`PLEDGE_LEGACY_ENGINE` only).
        // Previously-built outputs are put back into the function cache under
        // their path-based key, so the normal cache lookup in Phase 3a hits
        // for unchanged modules and misses (→ re-transform) for changed ones.
        // Everything here is keyed by *path*: module ids are assigned in
        // discovery order and shift between builds, so comparing ids across
        // builds mixed up unrelated modules.
        if !task_driven
            && self.is_incremental
            && let Some(prev) = self.previous_graph.take()
        {
            let mut preloaded = 0usize;
            // Use git tree hash for fast repo-level change detection
            if let Some(ref git) = self.git_invalidator
                && !git.has_repo_changed(prev.git_tree_hash.as_deref())
            {
                info!("Git tree hash unchanged — full cache hit");
                for node in prev.modules.values() {
                    if self.preload_cached(node.content_hash, &node.path) {
                        preloaded += 1;
                    }
                }
            }
            // If git invalidation isn't available or tree changed,
            // use content-hash-based incremental detection
            if preloaded == 0 {
                let current: Vec<(PathBuf, u64)> = self
                    .modules
                    .values()
                    .map(|m| (m.path.clone(), m.content_hash))
                    .collect();
                for (path, hash) in unchanged_prev_modules(&prev, &current) {
                    if self.preload_cached(hash, &path) {
                        preloaded += 1;
                    }
                }
                info!("Incremental: {} modules preloaded from cache", preloaded);
            }
            self.previous_graph = Some(prev);
        }

        // Phase 3a: BFS Resolution — discover all modules using fast SIMD scanning.
        // Deps come from find_imports + extract_module_specifier, NOT from Oxc transform.
        // Uncached modules are collected for parallel transformation in Phase 3b.
        let mut modules_built = 0usize;
        let mut modules_cached = 0usize;
        let mut pending_transforms: Vec<(ModuleId, ResolvedModule)> = Vec::new();

        // Frontier-based BFS: each level resolves all outstanding dep
        // specifiers, then batch-reads every newly discovered file in ONE
        // FFI call (IOCP on Windows, io_uring on Linux) instead of a serial
        // read per file. Deterministic ordering is preserved by sorting each
        // level's ids — critical for reproducible output hashes.
        let mut frontier: Vec<ModuleId> = self.path_to_id.values().copied().collect();
        frontier.sort_unstable();
        frontier.dedup();
        let mut processed = HashSet::new();
        // (specifier, importer) → resolution, so plugin `resolveId` hooks
        // run once per distinct import rather than once per pass.
        let mut resolve_memo: HashMap<(String, PathBuf), Resolution> = HashMap::new();

        while !frontier.is_empty() {
            // Phase A: process this level's modules (sources already in
            // memory — no file I/O), collecting dep specifiers.
            let mut dep_specs: Vec<(String, PathBuf)> = Vec::new();
            for module_id in frontier.drain(..) {
                if !processed.insert(module_id) {
                    continue;
                }

                let module = match self.modules.get(&module_id) {
                    Some(m) => m.clone(),
                    None => continue,
                };

                self.module_graph.add_module(
                    module_id,
                    module.path.clone(),
                    module.kind,
                    module.content_hash,
                );

                let cache_key = module_cache_key(module.content_hash, &module.path);

                // Check function-level cache (memory first, then disk, then remote)
                if let Some(cached) = self
                    .function_cache
                    .get(&cache_key)
                    .map(|r| r.value().clone())
                {
                    modules_cached += 1;
                    for dep_path in &cached.deps {
                        dep_specs.push((dep_path.clone(), module.path.clone()));
                    }
                    continue;
                }

                if !task_driven && let Some(ref pc) = self.persistent_cache {
                    let pkey = pledgepack_cache::make_key(
                        module.content_hash,
                        "transform",
                        &module.path.to_string_lossy().to_string(),
                    );
                    if let Some(entry) = pc.get(&pkey) {
                        modules_cached += 1;
                        let cached = CachedOutput {
                            code: entry.code,
                            source_map: entry.source_map,
                            deps: entry.deps,
                            is_css: false,
                            css_modules: None,
                            extracted_css: None,
                            is_worker: false,
                            dynamic_imports: Vec::new(),
                        };
                        self.function_cache.insert(cache_key, cached.clone());
                        for dep_path in &cached.deps {
                            dep_specs.push((dep_path.clone(), module.path.clone()));
                        }
                        continue;
                    }

                    if let Some(ref rc) = self.remote_cache {
                        // Try remote cache before transforming
                        let rkey = pledgepack_cache::remote::remote_cache_key(
                            module.content_hash,
                            "transform",
                            module.path.to_string_lossy().as_ref(),
                        );
                        if let Ok(Some(remote_entry)) = rc.get(&rkey) {
                            modules_cached += 1;
                            debug!("Remote cache hit: {:?}", module.path);
                            let cached = CachedOutput {
                                code: remote_entry.code,
                                source_map: remote_entry.source_map,
                                deps: remote_entry.deps,
                                is_css: false,
                                css_modules: None,
                                extracted_css: None,
                                is_worker: false,
                                dynamic_imports: Vec::new(),
                            };
                            // Populate local caches
                            self.function_cache.insert(cache_key, cached.clone());
                            pc.set(
                                pkey,
                                pledgepack_cache::CacheEntry {
                                    code: cached.code.clone(),
                                    source_map: cached.source_map.clone(),
                                    deps: cached.deps.clone(),
                                    created_at: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                    version: pledgepack_cache::CACHE_FORMAT_VERSION,
                                },
                            );
                            for dep_path in &cached.deps {
                                dep_specs.push((dep_path.clone(), module.path.clone()));
                            }
                            continue;
                        }
                    }
                }

                // Not in any cache — discover deps via SIMD scanning for BFS, defer transform
                let source_str = String::from_utf8_lossy(&module.source).to_string();
                let (_, import_offsets) = pledgepack_native_sys::summarize_module(&module.source);
                for offset in import_offsets {
                    if let Some(dep) = extract_module_specifier(&source_str, offset) {
                        dep_specs.push((dep, module.path.clone()));
                    }
                }
                pending_transforms.push((module_id, module));
            }

            // Phase B: resolve every collected specifier to a path, dedupe
            // against already-registered modules, then batch-read all new
            // files in one FFI call (IOCP / io_uring / thread pool).
            let mut new_paths: Vec<PathBuf> = Vec::new();
            let mut seen_this_level: HashSet<PathBuf> = HashSet::new();
            let mut next_ids: Vec<ModuleId> = Vec::new();
            for (spec, importer) in dep_specs {
                let path = match self.resolve_memo(&mut resolve_memo, &spec, &importer, runner)? {
                    Resolution::External => continue,
                    Resolution::File(p) => p,
                };
                if let Some(&id) = self.path_to_id.get(&path) {
                    if !processed.contains(&id) {
                        next_ids.push(id);
                    }
                    continue;
                }
                if seen_this_level.insert(path.clone()) {
                    new_paths.push(path);
                }
            }
            if !new_paths.is_empty() {
                // Plugin `load` hook first (first non-null wins); everything
                // it declines is batch-read in one FFI call (IOCP / io_uring
                // / thread pool).
                let mut loaded: HashMap<PathBuf, Vec<u8>> = HashMap::new();
                let mut to_read: Vec<PathBuf> = Vec::new();
                for path in &new_paths {
                    match self.plugin_load(path, runner)? {
                        Some(src) => {
                            loaded.insert(path.clone(), src);
                        }
                        None => to_read.push(path.clone()),
                    }
                }
                let path_strs: Vec<&str> =
                    to_read.iter().map(|p| p.to_str().unwrap_or("")).collect();
                let read = pledgepack_native_sys::read_files_batch(&path_strs);
                for (path, src) in to_read.iter().zip(read) {
                    let source = src.map_err(|e| anyhow::anyhow!("{}: {}", crate::display_path(&path), e))?;
                    loaded.insert(path.clone(), source);
                }
                for path in new_paths {
                    let source = loaded.remove(&path).unwrap_or_default();
                    let id = self.add_module_with_source(path, source, runner)?;
                    next_ids.push(id);
                }
            }
            next_ids.sort_unstable();
            next_ids.dedup();
            frontier = next_ids;
        }

        // Phase 3b: Transform all uncached modules in parallel
        if !pending_transforms.is_empty() {
            modules_built += pending_transforms.len();
            let parallel_results = if task_driven {
                let (results, hits) = self.transform_modules_via_tasks(pending_transforms)?;
                // Task-cache hits were counted as "built" above — correct
                // the metrics so cached vs computed is reported honestly.
                modules_built -= hits;
                modules_cached += hits;
                results
            } else {
                self.transform_modules_parallel(pending_transforms)?
            };

            // Phase 3c: Populate caches from parallel results
            for (module_id, output) in parallel_results {
                let Some(module) = self.modules.get(&module_id) else {
                    warn!(
                        "Module {:?} vanished between transform and cache write",
                        module_id
                    );
                    continue;
                };
                let cache_key = module_cache_key(module.content_hash, &module.path);

                if !task_driven && let Some(ref pc) = self.persistent_cache {
                    let pkey = pledgepack_cache::make_key(
                        module.content_hash,
                        "transform",
                        &module.path.to_string_lossy().to_string(),
                    );
                    pc.set(
                        pkey,
                        pledgepack_cache::CacheEntry {
                            code: output.code.clone(),
                            source_map: output.source_map.clone(),
                            deps: output.deps.clone(),
                            created_at: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                            version: pledgepack_cache::CACHE_FORMAT_VERSION,
                        },
                    );
                }

                if !task_driven && let Some(ref rc) = self.remote_cache {
                    let rkey = pledgepack_cache::remote::remote_cache_key(
                        module.content_hash,
                        "transform",
                        module.path.to_string_lossy().as_ref(),
                    );
                    if let Err(e) = rc.set(
                        &rkey,
                        &pledgepack_cache::remote::RemoteCacheEntry {
                            code: output.code.clone(),
                            source_map: output.source_map.clone(),
                            deps: output.deps.clone(),
                            created_at: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        },
                    ) {
                        tracing::debug!("Remote cache store failed: {}", e);
                    }
                }

                self.function_cache.insert(cache_key, output);
            }
        }

        // Phase 3d: Wire up dependency graph for all modules (cached + transformed)
        for (&module_id, module) in &self.modules {
            if let Some(cached) = self
                .function_cache
                .get(&module_cache_key(module.content_hash, &module.path))
            {
                for dep_path in &cached.deps {
                    if let Ok(Resolution::File(dep_path_resolved)) =
                        self.resolve_memo(&mut resolve_memo, dep_path, &module.path, runner)
                        && let Some(&dep_id) = self.path_to_id.get(&dep_path_resolved)
                    {
                        self.graph
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .add_dependency(module_id, dep_id);
                        self.module_graph.add_dependency(module_id, dep_id);
                    }
                }
            }
        }

        // Phase 4: Save module graph to disk for next incremental build
        if self.config.cache.enabled {
            let graph_path = self.config.cache.dir.join("module_graph.bin");
            self.module_graph.built_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Store git tree hash for fast invalidation on next build
            if let Some(ref git) = self.git_invalidator {
                self.module_graph.git_tree_hash = git.root_tree_hash().map(|s| s.to_string());
            }
            if let Err(e) = self.module_graph.save_to_disk(&graph_path) {
                warn!("Failed to save module graph: {}", e);
            }
        }

        let duration = start.elapsed();

        info!(
            "Build complete: {} built, {} cached, {}ms{}",
            modules_built,
            modules_cached,
            duration.as_millis(),
            if self.is_incremental {
                " (incremental)"
            } else {
                ""
            }
        );

        Ok(BuildResult {
            modules_built,
            modules_cached,
            duration_ms: duration.as_millis(),
        })
    }

    /// Try to load a cached module output (memory, then persistent cache).
    fn load_cached_module(&self, content_hash: u64, path: &Path) -> Option<CachedOutput> {
        if let Some(cached) = self
            .function_cache
            .get(&module_cache_key(content_hash, path))
        {
            return Some(cached.clone());
        }
        if let Some(ref pc) = self.persistent_cache {
            let pkey = pledgepack_cache::make_key(
                content_hash,
                "transform",
                &path.to_string_lossy().to_string(),
            );
            if let Some(entry) = pc.get(&pkey) {
                return Some(CachedOutput {
                    code: entry.code,
                    source_map: entry.source_map,
                    deps: entry.deps,
                    is_css: false,
                    css_modules: None,
                    extracted_css: None,
                    is_worker: false,
                    dynamic_imports: Vec::new(),
                });
            }
        }
        None
    }

    /// Put a previously built module output into the function cache under
    /// its path-based key. Returns whether anything was found.
    fn preload_cached(&self, content_hash: u64, path: &Path) -> bool {
        match self.load_cached_module(content_hash, path) {
            Some(cached) => {
                self.function_cache
                    .insert(module_cache_key(content_hash, path), cached);
                true
            }
            None => false,
        }
    }

    /// Run the plugin `transform` chain over `text`, through the plugin
    /// transform cache when the whole chain is fingerprintable.
    ///
    /// The key covers the chain fingerprint (plugin identity, version, source
    /// and configuration - so upgrading or reconfiguring a plugin can never
    /// serve a stale result), the module id and the exact input source. Both
    /// hits and "nothing changed" results are cached, in memory and - when a
    /// persistent cache is configured - on disk.
    fn cached_plugin_transform(
        &mut self,
        hooks: &HookRunner<'_>,
        text: &str,
        id: &str,
    ) -> Result<Option<crate::plugin_hooks::CodeResult>> {
        /// Marker stored in `CacheEntry::deps` for "the chain left the code unchanged".
        const UNCHANGED: &str = "\u{0}plugin-transform-unchanged";

        let Some(fingerprint) = hooks.transform_fingerprint() else {
            return hooks.transform(text, id);
        };
        let mut hasher = blake3::Hasher::new();
        hasher.update(fingerprint.as_bytes());
        hasher.update(&[0]);
        hasher.update(id.as_bytes());
        hasher.update(&[0]);
        hasher.update(text.as_bytes());
        let key: [u8; 32] = *hasher.finalize().as_bytes();

        if let Some(hit) = self.plugin_transform_cache.get(&key) {
            return Ok(hit.clone());
        }
        let pkey = self.persistent_cache.as_ref().map(|_| {
            pledgepack_cache::make_key(
                u64::from_be_bytes(key[0..8].try_into().unwrap()),
                "plugin-transform",
                &blake3::Hash::from_bytes(key).to_hex().to_string(),
            )
        });
        if let (Some(pc), Some(pkey)) = (self.persistent_cache.as_ref(), pkey.as_ref())
            && let Some(entry) = pc.get(pkey)
        {
            let out = if entry.deps.iter().any(|d| d == UNCHANGED) {
                None
            } else {
                Some(crate::plugin_hooks::CodeResult {
                    code: entry.code,
                    map: entry.source_map,
                })
            };
            self.plugin_transform_cache.insert(key, out.clone());
            return Ok(out);
        }

        let out = hooks.transform(text, id)?;
        if let (Some(pc), Some(pkey)) = (self.persistent_cache.as_ref(), pkey) {
            let (code, source_map, deps) = match &out {
                Some(r) => (r.code.clone(), r.map.clone(), Vec::new()),
                None => (String::new(), None, vec![UNCHANGED.to_string()]),
            };
            pc.set(
                pkey,
                pledgepack_cache::CacheEntry {
                    code,
                    source_map,
                    deps,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    version: pledgepack_cache::CACHE_FORMAT_VERSION,
                },
            );
        }
        self.plugin_transform_cache.insert(key, out.clone());
        Ok(out)
    }

    /// The source map to emit for `module`: the built-in transform's map,
    /// composed with the plugin transform chain's map when plugins rewrote
    /// the source (so it points at the file on disk, not the plugin output).
    /// Falls back to the built-in map when composition is impossible.
    fn effective_source_map(
        &self,
        module: &ResolvedModule,
        cached: &CachedOutput,
    ) -> Option<String> {
        let outer = cached.source_map.as_ref()?;
        match self.plugin_source_maps.get(&module.id) {
            Some(inner) => Some(
                crate::sourcemap_compose::compose_source_maps(outer, inner)
                    .unwrap_or_else(|| outer.clone()),
            ),
            None => Some(outer.clone()),
        }
    }

    /// The cached, transformed output of `module`.
    fn cached_for(
        &self,
        module: &ResolvedModule,
    ) -> Option<dashmap::mapref::one::Ref<'_, u64, CachedOutput>> {
        self.function_cache
            .get(&module_cache_key(module.content_hash, &module.path))
    }

    /// Resolve a module path and add it to the graph.
    /// `importer` is the path of the importing module (for relative resolution).
    fn resolve_and_add(
        &mut self,
        specifier: &str,
        importer: Option<&PathBuf>,
        hooks: Option<&HookRunner<'_>>,
    ) -> Result<ModuleId> {
        let path = match self.resolve_hooked(specifier, importer, hooks)? {
            Resolution::File(p) => p,
            Resolution::External => {
                bail!("entry '{specifier}' was marked external by a plugin `resolveId` hook")
            }
        };

        if self.path_to_id.contains_key(&path) {
            return Ok(self.path_to_id[&path]);
        }

        let source = match self.plugin_load(&path, hooks)? {
            Some(src) => src,
            // Read source via Zig I/O layer
            None => pledgepack_native_sys::read_file(path.to_str().unwrap_or(""))?,
        };
        self.add_module_with_source(path, source, hooks)
    }

    /// Plugin `resolveId` first, then the built-in resolver.
    fn resolve_hooked(
        &self,
        specifier: &str,
        importer: Option<&PathBuf>,
        hooks: Option<&HookRunner<'_>>,
    ) -> Result<Resolution> {
        if let Some(h) = hooks {
            let imp = importer.map(|p| crate::normalize_path(p));
            if let Some(r) = h.resolve_id(specifier, imp.as_deref())? {
                if r.external {
                    return Ok(Resolution::External);
                }
                return Ok(Resolution::File(self.plugin_id_to_path(&r.id)));
            }
        }
        self.resolve(specifier, importer).map(Resolution::File)
    }

    /// [`resolve_hooked`](Self::resolve_hooked) memoised per (specifier, importer).
    fn resolve_memo(
        &self,
        memo: &mut HashMap<(String, PathBuf), Resolution>,
        specifier: &str,
        importer: &PathBuf,
        hooks: Option<&HookRunner<'_>>,
    ) -> Result<Resolution> {
        let key = (specifier.to_string(), importer.clone());
        if let Some(r) = memo.get(&key) {
            return Ok(r.clone());
        }
        let r = self.resolve_hooked(specifier, Some(importer), hooks)?;
        memo.insert(key, r.clone());
        Ok(r)
    }

    /// Map an id returned by a plugin to a module path: an absolute path is
    /// taken as-is, a project-relative path is joined onto the root when the
    /// file exists, anything else is a virtual id kept verbatim.
    fn plugin_id_to_path(&self, id: &str) -> PathBuf {
        let p = PathBuf::from(id);
        if p.is_absolute() {
            return p;
        }
        let joined = self.config.root.join(id);
        if joined.is_file() { joined } else { p }
    }

    /// Run the plugin `load` hook for `path`. `Ok(None)` means "read the
    /// file". A path that is not a file and that no plugin loads is an error
    /// with a clear message rather than a bare "No such file".
    fn plugin_load(&self, path: &Path, hooks: Option<&HookRunner<'_>>) -> Result<Option<Vec<u8>>> {
        let Some(h) = hooks else {
            return Ok(None);
        };
        let id = crate::normalize_path(path);
        if let Some(r) = h.load(&id)? {
            return Ok(Some(r.code.into_bytes()));
        }
        if !path.is_file() {
            bail!(
                "module '{id}' was resolved by a plugin but is not a file, and no plugin `load` hook returned code for it"
            );
        }
        Ok(None)
    }

    /// Register a module whose source is already in memory (e.g. batch-read
    /// by the frontier loader). Runs the plugin `transform` chain over the
    /// source, assigns a graph id, records the resolved module, and enforces
    /// module-count limits.
    fn add_module_with_source(
        &mut self,
        path: PathBuf,
        source: Vec<u8>,
        hooks: Option<&HookRunner<'_>>,
    ) -> Result<ModuleId> {
        if let Some(&id) = self.path_to_id.get(&path) {
            return Ok(id);
        }

        let mut kind = ModuleKind::from_path(&path);
        // A virtual module (no file behind it) without a recognisable
        // extension is JavaScript.
        if kind == ModuleKind::Unknown && hooks.is_some() && !path.is_file() {
            kind = ModuleKind::JavaScript;
        }

        // Plugin `transform` chain — text modules only (binary assets are not
        // valid UTF-8 and must not be round-tripped through a JS string).
        let mut source = source;
        let mut plugin_map: Option<String> = None;
        if let Some(h) = hooks
            && !matches!(kind, ModuleKind::Asset | ModuleKind::Wasm)
            && let Ok(text) = std::str::from_utf8(&source)
            && let Some(r) = self.cached_plugin_transform(h, text, &crate::normalize_path(&path))?
        {
            plugin_map = r.map;
            source = r.code.into_bytes();
        }

        let id = self
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add_module(path.to_str().unwrap_or(""));
        self.path_to_id.insert(path.clone(), id);
        match plugin_map {
            Some(m) => {
                self.plugin_source_maps.insert(id, m);
            }
            None => {
                self.plugin_source_maps.remove(&id);
            }
        }

        let content_hash =
            u64::from_be_bytes(blake3::hash(&source).as_bytes()[0..8].try_into().unwrap());

        let module = ResolvedModule {
            id,
            path: path.clone(),
            kind,
            source,
            content_hash,
        };

        self.modules.insert(id, module);

        // Guard against unbounded memory growth: every resolved module's
        // full source is kept in `self.modules` for the rest of the build.
        let module_count = self.modules.len();
        if module_count > MAX_MODULES {
            bail!(
                "Module count exceeded maximum ({}). This likely indicates a bug in module resolution.",
                MAX_MODULES
            );
        }
        // Warn on threshold crossings rather than every insertion past the
        // limit, to avoid flooding the log on large builds.
        if module_count == WARN_MODULES + 1
            || (module_count > WARN_MODULES && module_count.is_multiple_of(WARN_MODULES))
        {
            warn!(
                "Large module count ({}). Consider increasing memory limits or splitting the build.",
                module_count
            );
        }

        Ok(id)
    }

    /// Export conditions used for package `exports` resolution, in priority
    /// order: the configured conditions, then the bundler-standard `module`.
    fn export_conditions(&self) -> Vec<String> {
        let mut conds = self.config.conditions.clone();
        if !conds.iter().any(|c| c == "module") {
            conds.push("module".to_string());
        }
        conds
    }

    /// Resolve a `#specifier` through the `imports` field of the importer's
    /// package (the nearest `package.json` at or above the importing file, as
    /// in Node: package scope ends at the first `package.json`, even one
    /// without `imports`). Targets are package-relative files (extension and
    /// index probing applies) or bare package specifiers resolved as usual.
    fn resolve_package_import(
        &self,
        specifier: &str,
        importer: Option<&PathBuf>,
    ) -> Result<PathBuf> {
        let start = importer
            .and_then(|p| p.parent())
            .unwrap_or(&self.config.root)
            .to_path_buf();
        let mut current = start.clone();
        loop {
            let pkg_json = current.join("package.json");
            if pkg_json.is_file() {
                let content = std::fs::read_to_string(&pkg_json)?;
                let pkg: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| anyhow::anyhow!("invalid {}: {e}", crate::display_path(&pkg_json)))?;
                let Some(imports) = pkg.get("imports").and_then(|v| v.as_object()) else {
                    bail!(
                        "Package import '{specifier}' is not defined: {} has no \"imports\" field",
                        crate::display_path(&pkg_json)
                    );
                };
                let conditions = self.export_conditions();
                return match crate::package_map::resolve_imports_entry(
                    imports,
                    specifier,
                    &conditions,
                ) {
                    Some(target) if target.starts_with("./") => {
                        let full = current.join(target.trim_start_matches("./"));
                        resolve_file_like(&full, &self.config.extensions).ok_or_else(|| {
                            anyhow::anyhow!(
                                "Package import '{specifier}' maps to '{target}' in {}, which does not exist",
                                crate::display_path(&pkg_json)
                            )
                        })
                    }
                    // A bare package target ("#dep": "dep-pkg").
                    Some(bare) => self.resolve(&bare, importer),
                    None => bail!(
                        "Package import '{specifier}' is not defined in {}",
                        crate::display_path(&pkg_json)
                    ),
                };
            }
            if !current.pop() {
                break;
            }
        }
        bail!(
            "Cannot resolve package import '{specifier}': no package.json found above {}",
            crate::display_path(&start)
        )
    }

    /// Resolve a module specifier to a file path.
    /// `importer` is the path of the importing module (for relative resolution).
    fn resolve(&self, specifier: &str, importer: Option<&PathBuf>) -> Result<PathBuf> {
        // 0a. Virtual modules: /__pledge_router → .pledge/gen/__pledge_router.tsx
        if specifier == "/__pledge_router" {
            let gen_router = self
                .config
                .root
                .join(".pledge")
                .join("gen")
                .join("__pledge_router.tsx");
            if gen_router.exists() {
                return Ok(gen_router);
            }
            // Fallback: check out_dir for CLI-generated router
            let out_router = self
                .config
                .root
                .join(&self.config.out_dir)
                .join("__pledge_router.js");
            if out_router.exists() {
                return Ok(out_router);
            }
        }

        // 0. Check path aliases (e.g., "@/components" → "src/components").
        // `alias` (tsconfig-style) and `resolve_alias` (`resolve.alias` in the
        // config file) are both honoured — the dev server already reads
        // `resolve_alias`, so the production resolver must too.
        for alias in self
            .config
            .alias
            .iter()
            .chain(self.config.resolve_alias.iter())
        {
            if specifier.starts_with(&alias.from) {
                let rest = &specifier[alias.from.len()..];
                // Ensure we match at a path boundary: alias "@/components" should not match "@/components-extra"
                if !rest.is_empty() && !rest.starts_with('/') && !alias.from.ends_with('/') {
                    continue;
                }
                let mut alias_path = PathBuf::from(&alias.to);
                // Relative alias targets are relative to the project root,
                // not the process working directory.
                if alias_path.is_relative() {
                    alias_path = self.config.root.join(alias_path);
                }
                let path = if rest.is_empty() {
                    alias_path
                } else {
                    alias_path.join(rest.trim_start_matches('/'))
                };
                if let Some(found) = resolve_file_like(&path, &self.config.extensions) {
                    return Ok(found);
                }
            }
        }

        // Package `imports` (`#specifier`), scoped to the importer's package.
        if specifier.starts_with('#') {
            return self.resolve_package_import(specifier, importer);
        }

        // Handle relative paths
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let base = importer
                .and_then(|p| p.parent())
                .unwrap_or(&self.config.root);
            // Normalize `.`/`..` components out of the joined path instead of
            // leaving them in place: `base.join("./x")` yields `base/./x`,
            // and `\\?\`-prefixed roots (Windows verbatim paths, produced by
            // `canonicalize`) never collapse `.`/`..`, so `exists()` fails
            // even when the file is present.
            let path = {
                let mut p = base.to_path_buf();
                for comp in std::path::Path::new(specifier).components() {
                    match comp {
                        std::path::Component::CurDir => {}
                        std::path::Component::ParentDir => {
                            p.pop();
                        }
                        other => p.push(other.as_os_str()),
                    }
                }
                p
            };

            if let Some(found) = resolve_file_like(&path, &self.config.extensions) {
                return Ok(found);
            }
        }

        // Handle bare specifiers (node_modules) — walk up directory tree for monorepo support
        if !specifier.starts_with('.') && !specifier.starts_with('/') {
            // Handle subpath imports: "react-dom/client" → "react-dom" + "/client"
            let (pkg_name, subpath) = if specifier.starts_with('@') {
                // Scoped: @org/pkg/sub → (@org/pkg, /sub)
                let parts: Vec<&str> = specifier.splitn(3, '/').collect();
                if parts.len() >= 2 {
                    let pkg = format!("{}/{}", parts[0], parts[1]);
                    let sub = if parts.len() == 3 {
                        format!("/{}", parts[2])
                    } else {
                        String::new()
                    };
                    (pkg, sub)
                } else {
                    (specifier.to_string(), String::new())
                }
            } else {
                // Non-scoped: pkg/sub → (pkg, /sub)
                match specifier.split_once('/') {
                    Some((pkg, sub)) => (pkg.to_string(), format!("/{}", sub)),
                    None => (specifier.to_string(), String::new()),
                }
            };

            let mut current = self.config.root.clone();
            loop {
                let node_modules = current.join("node_modules");

                let pkg_dir = node_modules.join(&pkg_name);
                let pkg_json = pkg_dir.join("package.json");

                if pkg_json.exists() {
                    let content = read_file_mmap(&pkg_json)?;
                    let pkg: serde_json::Value = serde_json::from_str(&content)?;
                    let conditions = self.export_conditions();

                    // "exports" (modern) takes precedence over everything else.
                    let export_key = if subpath.is_empty() {
                        ".".to_string()
                    } else {
                        format!(".{}", subpath)
                    };
                    if let Some(exports) = pkg.get("exports")
                        && let Some(target) =
                            resolve_package_exports(exports, &export_key, &conditions)
                    {
                        let full = pkg_dir.join(target.trim_start_matches("./"));
                        if full.is_file() {
                            return Ok(full);
                        }
                    }

                    if subpath.is_empty() {
                        // Fallback: resolve via "module" or "main"
                        let entry = pkg
                            .get("module")
                            .or_else(|| pkg.get("main"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("index.js");
                        let full = pkg_dir.join(entry.trim_start_matches("./"));
                        if let Some(found) = resolve_file_like(&full, &self.config.extensions) {
                            return Ok(found);
                        }
                        // Try index.js in the package directory
                        let index = pkg_dir.join("index.js");
                        if index.is_file() {
                            return Ok(index);
                        }
                        return Ok(full); // Return even if doesn't exist — error will surface on read
                    } else {
                        // Fallback: direct file path (with extension probing)
                        let direct = pkg_dir.join(subpath.trim_start_matches('/'));
                        if let Some(found) = resolve_file_like(&direct, &self.config.extensions) {
                            return Ok(found);
                        }
                    }
                }

                // Walk up to parent directory for hoisted node_modules
                if !current.pop() {
                    break;
                }
            }
        }

        // Last resort: try as-is
        let path = self.config.root.join(specifier);
        if path.exists() {
            return Ok(path);
        }

        anyhow::bail!("Cannot resolve module: {}", specifier)
    }

    // PRODUCTION-READINESS-100.md goal 91: the single-module `transform_module`
    // async method was removed from here — zero call sites, fully superseded
    // by `transform_modules_parallel` below (same transform/i18n/encrypt
    // pipeline, run across all modules via rayon instead of one at a time).

    /// Transform multiple modules in parallel using rayon.
    /// Returns transformed outputs keyed by module ID.
    /// Respects build.parallel config (#120) to limit concurrency.
    pub fn transform_modules_parallel(
        &mut self,
        modules: Vec<(ModuleId, ResolvedModule)>,
    ) -> Result<Vec<(ModuleId, CachedOutput)>> {
        let is_production = self.config.mode == crate::config::BuildMode::Production;
        let config = self.config.clone();

        // Feature 120: Build concurrency control
        let parallelism = crate::advanced::determine_parallelism(config.build.parallel);
        let pool = build_transform_pool(parallelism)?;

        let results: Vec<
            Result<(
                ModuleId,
                CachedOutput,
                Option<crate::i18n::TranslationCatalog>,
            )>,
        > = pool.install(|| {
            modules
                .par_iter()
                .map(|(id, module)| {
                    let source_str = String::from_utf8_lossy(&module.source).to_string();
                    let (_, import_offsets) =
                        pledgepack_native_sys::summarize_module(&module.source);

                    let mut deps = Vec::new();
                    for offset in import_offsets {
                        if let Some(dep) = extract_module_specifier(&source_str, offset) {
                            deps.push(dep);
                        }
                    }

                    let file_path = module.path.to_str().unwrap_or("");
                    let transform_output = crate::transform::transform(
                        &source_str,
                        module.kind,
                        file_path,
                        is_production,
                        &config,
                    )?;

                    // i18n-aware bundling: transform locale imports (#106)
                    let code = if config.i18n.enabled {
                        crate::i18n::transform_i18n_imports(&transform_output.code, &config.i18n)
                    } else {
                        transform_output.code
                    };

                    // i18n key extraction (#13): extract t('key') calls from TSX/TS/JSX
                    let i18n_keys = if config.i18n.enabled && config.i18n.extract {
                        let extraction = crate::i18n::extract_i18n_keys(&source_str, file_path);
                        if !extraction.keys.is_empty() {
                            let mut catalog = crate::i18n::TranslationCatalog::default();
                            for key in extraction.keys {
                                let key_str = key.key.clone();
                                catalog.keys.entry(key_str).or_default().push(key);
                            }
                            Some(catalog)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Build-time string encryption (#109)
                    let code = if config.encrypt.enabled {
                        crate::encrypt::encrypt_strings(&code, &config.encrypt)
                            .map(|(c, _)| c)
                            .unwrap_or(code)
                    } else {
                        code
                    };

                    Ok((
                        *id,
                        CachedOutput {
                            code,
                            source_map: transform_output.source_map,
                            deps,
                            is_css: transform_output.is_css,
                            css_modules: transform_output.css_modules,
                            extracted_css: transform_output.extracted_css,
                            is_worker: transform_output.is_worker,
                            dynamic_imports: transform_output.dynamic_imports,
                        },
                        i18n_keys,
                    ))
                })
                .collect()
        });

        // Collect results, propagating errors
        let mut outputs = Vec::with_capacity(results.len());
        for result in results {
            let (id, output, i18n_keys) = result?;
            if let Some(catalog) = i18n_keys {
                self.i18n_catalog.merge(catalog);
            }
            outputs.push((id, output));
        }
        Ok(outputs)
    }

    /// Transform modules through the task graph (the default pipeline).
    ///
    /// Each module becomes a content-addressed transform task — identical
    /// (source, kind, path, mode, config) inputs yield the same `TaskId`,
    /// so unchanged modules hit the task cache (memory → disk → remote)
    /// without re-running Oxc. Task reads run inside the same rayon pool
    /// the legacy path uses, honoring `build.parallel`.
    ///
    /// i18n and string encryption intentionally run OUTSIDE the memoized
    /// task boundary: `encrypt_strings` draws a fresh random key per call
    /// (goal 15) and must not be frozen into a cached output.
    pub fn transform_modules_via_tasks(
        &mut self,
        modules: Vec<(ModuleId, ResolvedModule)>,
    ) -> Result<(Vec<(ModuleId, CachedOutput)>, usize)> {
        let is_production = self.config.mode == crate::config::BuildMode::Production;
        let config = self.config.clone();
        let engine = self.task_engine.clone().ok_or_else(|| {
            anyhow::anyhow!("transform_modules_via_tasks requires the task engine to be enabled")
        })?;

        let parallelism = crate::advanced::determine_parallelism(config.build.parallel);
        let pool = build_transform_pool(parallelism)?;

        type TaskResult = Result<(
            ModuleId,
            CachedOutput,
            Option<crate::i18n::TranslationCatalog>,
            bool,
        )>;
        let results: Vec<TaskResult> = pool.install(|| {
            modules
                .par_iter()
                .map(|(id, module)| {
                    let source_str = String::from_utf8_lossy(&module.source).to_string();
                    let file_path = module.path.to_str().unwrap_or("").to_string();

                    let source_arc = Arc::new(source_str.clone());
                    let path_arc = Arc::new(file_path.clone());
                    let parse_id = crate::task_transform::TaskTransformEngine::parse_task_id(
                        &source_str,
                        module.kind,
                        &file_path,
                    );
                    engine.register_parse_task(source_arc.clone(), module.kind, path_arc.clone());
                    let task = engine.register_transform_task(
                        source_arc,
                        module.kind,
                        path_arc,
                        is_production,
                        config.clone(),
                        parse_id,
                    );
                    // Memory-or-disk check BEFORE the read so build metrics
                    // distinguish real computes from task-cache hits.
                    let was_cached = engine.engine().is_cached(&task.id());
                    let task_output = engine.read_transform_blocking(task)?;

                    // i18n-aware bundling: transform locale imports (#106)
                    let code = if config.i18n.enabled {
                        crate::i18n::transform_i18n_imports(&task_output.code, &config.i18n)
                    } else {
                        task_output.code.clone()
                    };

                    // i18n key extraction (#13)
                    let i18n_keys = if config.i18n.enabled && config.i18n.extract {
                        let extraction = crate::i18n::extract_i18n_keys(&source_str, &file_path);
                        if !extraction.keys.is_empty() {
                            let mut catalog = crate::i18n::TranslationCatalog::default();
                            for key in extraction.keys {
                                let key_str = key.key.clone();
                                catalog.keys.entry(key_str).or_default().push(key);
                            }
                            Some(catalog)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Build-time string encryption (#109)
                    let code = if config.encrypt.enabled {
                        crate::encrypt::encrypt_strings(&code, &config.encrypt)
                            .map(|(c, _)| c)
                            .unwrap_or(code)
                    } else {
                        code
                    };

                    Ok((
                        *id,
                        CachedOutput {
                            code,
                            source_map: task_output.source_map.clone(),
                            deps: task_output.deps.clone(),
                            is_css: task_output.is_css,
                            css_modules: task_output.css_modules.clone(),
                            extracted_css: task_output.extracted_css.clone(),
                            is_worker: task_output.is_worker,
                            dynamic_imports: task_output.dynamic_imports.clone(),
                        },
                        i18n_keys,
                        was_cached,
                    ))
                })
                .collect()
        });

        let mut outputs = Vec::with_capacity(results.len());
        let mut cache_hits = 0usize;
        for result in results {
            let (id, output, i18n_keys, was_cached) = result?;
            if let Some(catalog) = i18n_keys {
                self.i18n_catalog.merge(catalog);
            }
            if was_cached {
                cache_hits += 1;
            }
            outputs.push((id, output));
        }
        Ok((outputs, cache_hits))
    }

    /// Get a locked reference to the module graph (for dev server / HMR).
    /// Returns a `MutexGuard` — the lock is held for the guard's lifetime.
    pub fn graph(&self) -> std::sync::MutexGuard<'_, Graph> {
        self.graph.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Modules ordered by id. `self.modules` is a `HashMap`, so iterating it
    /// directly makes the order of emitted files, CSS `<link>` tags and entry
    /// `<script>` tags differ from run to run (CSS cascade order included).
    fn modules_sorted(&self) -> Vec<&ResolvedModule> {
        let mut v: Vec<&ResolvedModule> = self.modules.values().collect();
        v.sort_unstable_by_key(|m| m.id);
        v
    }

    /// Path of `module_path` relative to the project root, guaranteed to stay
    /// inside the output directory when joined. Modules outside the root (e.g.
    /// hoisted monorepo `node_modules`) used to keep their absolute path, and
    /// `out_dir.join(absolute)` *replaces* `out_dir` — emitting next to the
    /// sources instead of into the output directory.
    fn output_rel_path(&self, module_path: &Path) -> PathBuf {
        if let Ok(rel) = module_path.strip_prefix(&self.config.root) {
            return rel.to_path_buf();
        }
        let mut out = PathBuf::from("_external");
        for comp in module_path.components() {
            match comp {
                // Virtual plugin ids (`virtual:foo`) may contain characters
                // that are invalid in file names.
                std::path::Component::Normal(c) => {
                    out.push(sanitize_path_component(&c.to_string_lossy()))
                }
                std::path::Component::Prefix(p) => {
                    let s: String = p
                        .as_os_str()
                        .to_string_lossy()
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if !s.is_empty() {
                        out.push(s);
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Get all resolved modules
    pub fn modules(&self) -> &HashMap<ModuleId, ResolvedModule> {
        &self.modules
    }

    /// Get the entry module IDs (modules resolved from config entry points).
    /// Returns only the actual entry points, not all resolved modules.
    pub fn entry_ids(&self) -> Vec<ModuleId> {
        if !self.entry_module_ids.is_empty() {
            let mut ids = self.entry_module_ids.clone();
            ids.sort_unstable();
            ids.dedup();
            ids
        } else {
            // Fallback: if entry_module_ids wasn't populated (e.g., dev server),
            // derive from config entry + auto_entries
            let entries: Vec<String> = if !self.auto_entries.is_empty() {
                self.auto_entries.clone()
            } else {
                self.config.entry.clone()
            };
            entries
                .iter()
                .filter_map(|e| {
                    let path = self.config.root.join(e);
                    self.path_to_id.get(&path).copied()
                })
                .collect()
        }
    }

    /// Collect all module code into a single bundle string for edge bundle generation.
    /// Walks the dependency graph from entry points in dependency order.
    pub fn collect_bundle_code(&self) -> String {
        let mut bundle = String::new();
        let mut css_bundle = String::new();
        let mut visited = std::collections::HashSet::new();
        for entry_id in self.entry_ids() {
            self.collect_module_code(entry_id, &mut visited, &mut bundle, &mut css_bundle);
        }
        if !css_bundle.is_empty() {
            bundle.push_str("\n/* === CSS === */\n");
            bundle.push_str(&css_bundle);
        }
        bundle
    }

    /// Get the function-level cache (transformed outputs)
    pub fn function_cache(&self) -> &DashMap<u64, CachedOutput> {
        &self.function_cache
    }

    /// Get the extracted i18n translation catalog (#13)
    pub fn i18n_catalog(&self) -> &crate::i18n::TranslationCatalog {
        &self.i18n_catalog
    }

    /// Emit production build artifacts using optimizer-produced chunk groupings.
    ///
    /// Instead of writing each module as a separate file (the default `emit`
    /// behavior), this method concatenates all modules in each chunk into a
    /// single output file. This ensures the optimizer's code-splitting decisions
    /// (vendor chunks, shared chunks, entry chunks) are reflected in the output.
    ///
    /// Falls back to per-module `emit()` when `chunks` is empty, preserving
    /// backward compatibility for callers that don't use the optimizer.
    pub fn emit_with_chunks(&self, chunks: &[EmitChunk]) -> Result<()> {
        self.emit_with_chunks_hooks(chunks, None)
    }

    /// [`emit_with_chunks`](Self::emit_with_chunks) with plugin hooks.
    ///
    /// `renderChunk(code, "<chunk-id>.js", "entry" | "chunk")` runs on each
    /// chunk's concatenated code and `transformIndexHtml(html, "index.html")`
    /// on the generated page — both *before* content hashing / writing, so
    /// the hashes in file names and in the HTML reflect the final content.
    /// A plugin error aborts the emit with the plugin name and file.
    ///
    /// The chunk source map is a merged (indexed, `sections`) map of the
    /// per-module maps. If `renderChunk` changes the code the merged map no
    /// longer applies: the plugin's own map is used when it returns one,
    /// otherwise no map is written for that chunk.
    pub fn emit_with_chunks_hooks(
        &self,
        chunks: &[EmitChunk],
        hooks: Option<&dyn PluginHooks>,
    ) -> Result<()> {
        if chunks.is_empty() {
            return self.emit_with_hooks(hooks);
        }
        let runner = hooks.map(HookRunner::new);
        let runner = runner.as_ref();

        let out_dir = &self.config.out_dir;
        // Safety check: never delete the project root or source directories
        if out_dir == &self.config.root {
            anyhow::bail!("Output directory cannot be the same as project root");
        }
        // Acquire the output lock BEFORE wiping out_dir — two concurrent
        // builds writing to the same directory would corrupt each other.
        ensure_safe_out_dir(out_dir, &self.config.root, &self.config.entry)?;
        let _output_lock = OutputLock::acquire(out_dir)?;
        if out_dir.exists() {
            let canonical = out_dir.canonicalize().unwrap_or(out_dir.to_path_buf());
            if canonical.parent().is_none() {
                anyhow::bail!(
                    "Refusing to delete unsafe output directory: {}",
                    crate::display_path(&canonical)
                );
            }
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;

        let mut css_files: Vec<String> = Vec::new();
        let mut js_files: Vec<String> = Vec::new();
        let mut async_chunks: Vec<String> = Vec::new();
        let mut manifest_entries: std::collections::BTreeMap<String, ManifestEntry> =
            std::collections::BTreeMap::new();
        let mut entry_chunks: Vec<(String, String)> = Vec::new();

        // Determine entry modules from config or auto-discovered entries
        let entries: Vec<String> = if !self.auto_entries.is_empty() {
            self.auto_entries.clone()
        } else {
            self.config.entry.clone()
        };

        // Emit each chunk as a single concatenated file
        for chunk in chunks {
            let mut chunk_content = String::new();
            let mut chunk_css = String::new();
            let mut has_css = false;
            let mut chunk_dynamic_imports: Vec<String> = Vec::new();
            // (0-based line in the chunk where the module starts, module map)
            let mut map_sections: Vec<(usize, String)> = Vec::new();
            let mut line = 0usize;

            for &module_id in &chunk.modules {
                let module = self.modules.get(&module_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "chunk '{}' references module id {} that is not part of the build",
                        chunk.id,
                        module_id
                    )
                })?;
                let cached = self.cached_for(module).ok_or_else(|| {
                    anyhow::anyhow!(
                        "chunk '{}': transformed output for {} is missing from the cache — \
                         refusing to silently drop it from the bundle",
                        chunk.id,
                        crate::display_path(&module.path)
                    )
                })?;
                if cached.is_css {
                    chunk_css.push_str(&cached.code);
                    chunk_css.push('\n');
                    has_css = true;
                } else {
                    // An external per-module map is replaced by one merged
                    // chunk map, so the module's own trailing
                    // `sourceMappingURL` comment must go.
                    let code = if cached.source_map.is_some() {
                        strip_source_mapping_url(&cached.code)
                    } else {
                        cached.code.as_str()
                    };
                    if let Some(m) = self.effective_source_map(module, &cached) {
                        map_sections.push((line, m));
                    }
                    chunk_content.push_str(code);
                    chunk_content.push('\n');
                    line += code.matches('\n').count() + 1;
                    // Collect dynamic imports for manifest
                    for di in &cached.dynamic_imports {
                        if !chunk_dynamic_imports.contains(di) {
                            chunk_dynamic_imports.push(di.clone());
                        }
                    }
                    // Extract CSS from JS modules
                    if let Some(ref extracted_css) = cached.extracted_css {
                        chunk_css.push_str(extracted_css);
                        chunk_css.push('\n');
                        has_css = true;
                    }
                }
            }

            // Chunk ids come from the optimizer / user config (`manual_chunks`
            // names); never let one escape the output directory or produce an
            // invalid file name.
            let safe_id = sanitize_chunk_id(&chunk.id);

            // renderChunk runs BEFORE hashing so the hash reflects final code.
            let mut plugin_map: Option<String> = None;
            let mut code_rendered = false;
            if let Some(r) = runner {
                let chunk_type = if chunk.is_entry { "entry" } else { "chunk" };
                if let Some(res) =
                    r.render_chunk(&chunk_content, &format!("{safe_id}.js"), chunk_type)?
                {
                    chunk_content = res.code;
                    plugin_map = res.map;
                    code_rendered = true;
                }
            }

            // Compute content hash for the chunk filename
            let hash = blake3::hash(chunk_content.as_bytes());
            let hash_hex = &hash.to_hex()[..8];
            let chunk_filename = format!("{}.{}.js", safe_id, hash_hex);
            let chunk_path = out_dir.join(&chunk_filename);
            let chunk_rel = crate::normalize_path_str(&chunk_filename);

            // Create parent directories
            if let Some(parent) = chunk_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let chunk_map = if code_rendered {
                plugin_map
            } else {
                merge_chunk_source_maps(&chunk_filename, &map_sections)
            };
            let write_map = chunk_map.is_some() && self.config.build.source_map_mode != "hidden";

            // Write the chunk file (the trailing comment is not part of the hash)
            if write_map {
                let with_url =
                    format!("{chunk_content}//# sourceMappingURL={chunk_filename}.map\n");
                write_output_file(&chunk_path, &with_url)?;
            } else {
                write_output_file(&chunk_path, &chunk_content)?;
            }
            tracing::info!(
                "Emitted chunk: {} ({} modules)",
                crate::display_path(&chunk_path),
                chunk.modules.len()
            );

            if write_map && let Some(ref map) = chunk_map {
                std::fs::write(out_dir.join(format!("{chunk_filename}.map")), map)?;
            }

            // Write extracted CSS as a separate file if any
            if has_css && !chunk_css.is_empty() {
                let css_hash = blake3::hash(chunk_css.as_bytes());
                let css_hash_hex = &css_hash.to_hex()[..8];
                let css_filename = format!("{}.{}.css", safe_id, css_hash_hex);
                let css_path = out_dir.join(&css_filename);
                let css_rel = crate::normalize_path_str(&css_filename);
                std::fs::write(&css_path, &chunk_css)?;
                css_files.push(css_rel.clone());
                tracing::info!("Emitted chunk CSS: {}", crate::display_path(&css_path));
            }

            let is_entry = chunk.is_entry;
            let is_async = !chunk_dynamic_imports.is_empty()
                && !self.config.build.inline_dynamic_imports
                && !is_entry;

            if is_async {
                async_chunks.push(chunk_rel.clone());
            } else {
                js_files.push(chunk_rel.clone());
            }

            if is_entry {
                // Use chunk id as the entry name for HTML script tags
                entry_chunks.push((chunk.id.clone(), chunk_rel.clone()));
            }

            manifest_entries.insert(
                chunk.id.clone(),
                ManifestEntry {
                    file: chunk_rel.clone(),
                    is_entry,
                    is_css: false,
                    is_async,
                    imports: if !is_async {
                        chunk_dynamic_imports.clone()
                    } else {
                        Vec::new()
                    },
                    css: if has_css {
                        css_files.last().cloned()
                    } else {
                        None
                    },
                },
            );
        }

        // Generate manifest.json with entry-to-chunk mapping
        let manifest_json = serde_json::to_string_pretty(&manifest_entries)?;
        std::fs::write(out_dir.join("manifest.json"), manifest_json)?;

        // Write i18n translation catalog if enabled
        if self.config.i18n.enabled && self.config.i18n.extract && !self.i18n_catalog.is_empty() {
            let catalog_path = out_dir.join("i18n-catalog.json");
            std::fs::write(&catalog_path, self.i18n_catalog.to_json())?;
            info!(
                "i18n: extracted {} translation keys → {}",
                self.i18n_catalog.len(),
                crate::display_path(&catalog_path)
            );
        }

        // Generate index.html with CSS links and module script tags for entry chunks
        let css_links: String = css_files
            .iter()
            .map(|css| {
                format!(
                    r#"    <link rel="stylesheet" href="{}" />"#,
                    self.config.asset_url(css)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Module preload directives — strategy-based (#52)
        let module_preloads: String = match self.config.build.preload_strategy.as_str() {
            "manual" => String::new(),
            "eager" => {
                let mut chunks_to_preload: Vec<&String> = Vec::new();
                for (_, hashed) in &entry_chunks {
                    chunks_to_preload.push(hashed);
                }
                for chunk in &async_chunks {
                    if !chunks_to_preload.contains(&chunk) {
                        chunks_to_preload.push(chunk);
                    }
                }
                if self.config.build.module_preload {
                    chunks_to_preload
                        .iter()
                        .map(|chunk| {
                            format!(
                                r#"    <link rel="modulepreload" href="{}" />"#,
                                self.config.asset_url(chunk)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            }
            _ => {
                if self.config.build.module_preload {
                    entry_chunks
                        .iter()
                        .map(|(_, hashed)| {
                            format!(
                                r#"    <link rel="modulepreload" href="{}" />"#,
                                self.config.asset_url(hashed)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            }
        };

        // Build script tags for entry chunks
        let script_tags: String = if entry_chunks.is_empty() {
            if entries.is_empty() {
                tracing::warn!("No entry points configured — skipping script tag generation");
                String::new()
            } else {
                let entry = &entries[0];
                let entry_js = entry
                    .replace(".tsx", ".js")
                    .replace(".ts", ".js")
                    .replace(".jsx", ".js");
                let entry_hashed = manifest_entries
                    .values()
                    .find(|m| m.is_entry)
                    .map(|m| m.file.clone())
                    .unwrap_or(entry_js);
                format!(
                    r#"    <script type="module" src="{}"></script>"#,
                    self.config.asset_url(&entry_hashed)
                )
            }
        } else {
            entry_chunks
                .iter()
                .map(|(_, hashed)| {
                    format!(
                        r#"    <script type="module" src="{}"></script>"#,
                        self.config.asset_url(hashed)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Use project's index.html as template if it exists, otherwise generate default
        let project_html_path = self.config.root.join("index.html");
        let html = if project_html_path.exists() {
            if let Ok(template) = std::fs::read_to_string(&project_html_path) {
                // Strip the dev-mode entry `<script src="...">` tag(s) — the
                // template's index.html references the raw source entry
                // (e.g. `src="/src/index.tsx"`, transformed on the fly by
                // the dev server) so it can't be left in a production build:
                // the built output doesn't serve `/src/*` at all, and even
                // if it did, unstripped TSX isn't valid browser JS. Without
                // this, the emitted HTML below both this raw-source tag
                // (untouched) *and* the new hashed chunk's script tag,
                // producing a broken page that 404s trying to load the
                // former.
                let template = remove_entry_script_tags(&template, &entries);
                let mut html = template;
                let has_head = html.contains("</head>");
                let has_body = html.contains("</body>");

                if !css_links.is_empty() {
                    let injection = format!("{}\n", css_links);
                    if let Some(pos) = html.rfind("</head>") {
                        html.insert_str(pos, &injection);
                    } else if !has_head {
                        html.push_str(&injection);
                    }
                }
                if !module_preloads.is_empty() {
                    let injection = format!("{}\n", module_preloads);
                    if let Some(pos) = html.rfind("</head>") {
                        html.insert_str(pos, &injection);
                    } else if !has_head {
                        html.push_str(&injection);
                    }
                }
                if !script_tags.is_empty() {
                    let injection = format!("{}\n", script_tags);
                    if let Some(pos) = html.rfind("</body>") {
                        html.insert_str(pos, &injection);
                    } else if !has_body {
                        html.push_str(&injection);
                    }
                }
                html
            } else {
                Self::generate_default_html(&css_links, &module_preloads, &script_tags)
            }
        } else {
            Self::generate_default_html(&css_links, &module_preloads, &script_tags)
        };

        let html = match runner {
            Some(r) => r.transform_index_html(&html, "index.html")?,
            None => html,
        };
        let html_path = out_dir.join("index.html");
        std::fs::write(&html_path, html)?;
        info!("Generated: {}", crate::display_path(&html_path));

        Ok(())
    }

    /// Generate a default index.html when the project doesn't have one.
    fn generate_default_html(css_links: &str, module_preloads: &str, script_tags: &str) -> String {
        let mut html =
            String::from("<!DOCTYPE html>\n<html>\n  <head>\n    <meta charset=\"utf-8\" />\n");
        if !css_links.is_empty() {
            html.push_str(css_links);
            html.push('\n');
        }
        if !module_preloads.is_empty() {
            html.push_str(module_preloads);
            html.push('\n');
        }
        html.push_str("  </head>\n  <body>\n    <div id=\"root\"></div>\n");
        if !script_tags.is_empty() {
            html.push_str(script_tags);
            html.push('\n');
        }
        html.push_str("  </body>\n</html>\n");
        html
    }

    /// Emit production build artifacts to the output directory.
    /// Writes each module as a separate file with content hashes and generates index.html + manifest.json.
    /// Supports CSS code splitting, CSS extraction from JS, manual chunks, inline dynamic imports,
    /// module preload directives with configurable strategy (#52), preload/prefetch links,
    /// multi-script entry, build manifest, incremental output diff (#54), and build verification (#53).
    pub fn emit(&self) -> Result<()> {
        self.emit_with_hooks(None)
    }

    /// [`emit`](Self::emit) with plugin hooks: `renderChunk(code, "<rel path>", "module")`
    /// runs on every emitted module file and `transformIndexHtml` on the page,
    /// both before hashing / writing. A module whose code a `renderChunk`
    /// plugin changed gets no source map (the module map no longer applies).
    pub fn emit_with_hooks(&self, hooks: Option<&dyn PluginHooks>) -> Result<()> {
        let runner = hooks.map(HookRunner::new);
        let runner = runner.as_ref();
        let out_dir = &self.config.out_dir;
        // Safety check: never delete the project root or source directories
        if out_dir == &self.config.root {
            anyhow::bail!("Output directory cannot be the same as project root");
        }
        // Acquire the output lock BEFORE wiping out_dir (see emit_with_chunks).
        ensure_safe_out_dir(out_dir, &self.config.root, &self.config.entry)?;
        let _output_lock = OutputLock::acquire(out_dir)?;
        // Safety: refuse to delete root or filesystem root
        if out_dir.exists() {
            let canonical = out_dir.canonicalize().unwrap_or(out_dir.to_path_buf());
            if canonical.parent().is_none() {
                anyhow::bail!(
                    "Refusing to delete unsafe output directory: {}",
                    crate::display_path(&canonical)
                );
            }
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;

        let mut css_files: Vec<String> = Vec::new();
        let mut js_files: Vec<String> = Vec::new();
        let mut async_chunks: Vec<String> = Vec::new();
        let mut manifest_entries: std::collections::BTreeMap<String, ManifestEntry> =
            std::collections::BTreeMap::new();
        let mut entry_chunks: Vec<(String, String)> = Vec::new(); // (entry name, hashed filename)

        // Determine entry modules from config or auto-discovered entries
        let entries: Vec<String> = if !self.auto_entries.is_empty() {
            self.auto_entries.clone()
        } else {
            self.config.entry.clone()
        };

        // Write each transformed module to .pledge/
        for module in self.modules_sorted() {
            if let Some(cached) = self.cached_for(module) {
                // Determine output path relative to project root
                let rel = self.output_rel_path(&module.path);
                let out_path = out_dir.join(&rel);

                // renderChunk runs BEFORE hashing so the hash reflects final code.
                let rendered_owned: Option<String> = match runner {
                    Some(r) if !cached.is_css => r
                        .render_chunk(&cached.code, &crate::normalize_path(&rel), "module")?
                        .map(|res| res.code),
                    _ => None,
                };
                let code_rendered = rendered_owned.is_some();
                let code: &str = rendered_owned.as_deref().unwrap_or(&cached.code);

                // Compute content hash for filename
                let hash = blake3::hash(code.as_bytes());
                let hash_hex = &hash.to_hex()[..8];

                // CSS files keep .css extension, JS files get .js
                let (out_path, hashed_rel, is_css) = if cached.is_css {
                    let stem = out_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("index");
                    let hashed_name = format!("{}.{}.css", stem, hash_hex);
                    let p = out_path.with_file_name(hashed_name);
                    let rel = crate::normalize_path(p.strip_prefix(out_dir).unwrap_or(&p));
                    (p, rel, true)
                } else {
                    let stem = out_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("index");
                    let hashed_name = format!("{}.{}.js", stem, hash_hex);
                    let p = out_path.with_file_name(hashed_name);
                    let rel = crate::normalize_path(p.strip_prefix(out_dir).unwrap_or(&p));
                    (p, rel, false)
                };

                // Create parent directories
                if let Some(parent) = out_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                // Compute entry/async status early (needed for incremental output check)
                let original_rel = crate::normalize_path(&rel);
                let is_entry = entries.iter().any(|e| {
                    let entry_normalized = crate::normalize_path_str(e);
                    let entry_normalized = entry_normalized
                        .trim_start_matches("./")
                        .trim_start_matches('/');
                    // Match on a path-segment boundary: entry `index.tsx` must
                    // not claim `my-index.tsx` or `lib/other-index.tsx`.
                    original_rel == entry_normalized
                        || original_rel.ends_with(&format!("/{entry_normalized}"))
                });
                let is_async = !cached.is_css
                    && !cached.dynamic_imports.is_empty()
                    && !self.config.build.inline_dynamic_imports
                    && !is_entry;

                // Incremental output (#54): skip writing if file exists with identical content
                if self.config.build.incremental_output
                    && out_path.exists()
                    && let Ok(existing) = std::fs::read_to_string(&out_path)
                    && existing == code
                {
                    tracing::debug!("Skipped unchanged: {}", crate::display_path(&out_path));
                    // Still track for manifest/HTML
                    if is_css {
                        css_files.push(hashed_rel.clone());
                    } else {
                        if is_async {
                            async_chunks.push(hashed_rel.clone());
                        } else {
                            js_files.push(hashed_rel.clone());
                        }
                    }
                    if is_entry {
                        entry_chunks.push((original_rel.clone(), hashed_rel.clone()));
                    }
                    manifest_entries.insert(
                        original_rel.clone(),
                        ManifestEntry {
                            file: hashed_rel.clone(),
                            is_entry,
                            is_css,
                            is_async,
                            imports: if !is_css {
                                cached.dynamic_imports.clone()
                            } else {
                                Vec::new()
                            },
                            css: if is_css {
                                Some(hashed_rel.clone())
                            } else {
                                None
                            },
                        },
                    );
                    continue;
                }

                // Write the transformed code using mmap for large files
                write_output_file(&out_path, code)?;
                tracing::info!("Emitted: {}", crate::display_path(&out_path));

                // Write source map if present (respecting source_map_mode)
                if !code_rendered
                    && let Some(source_map) = self.effective_source_map(module, &cached)
                {
                    let mode = &self.config.build.source_map_mode;
                    if mode != "hidden" {
                        let map_path = out_path.with_extension(format!(
                            "{}.map",
                            out_path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("js")
                        ));
                        std::fs::write(&map_path, source_map)?;
                    }
                }

                // Track CSS files for HTML injection
                if is_css {
                    css_files.push(hashed_rel.clone());

                    // RTL CSS auto-generation for standalone CSS files (#107)
                    if crate::rtl::should_generate_rtl(&self.config.css) {
                        let css_content = std::fs::read_to_string(&out_path).unwrap_or_default();
                        if let Some(rtl_css) =
                            crate::rtl::generate_rtl_css(&css_content, &self.config.css)
                        {
                            let rtl_path = out_path.with_extension({
                                let ext = out_path
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .unwrap_or("css");
                                format!("{}.rtl", ext)
                            });
                            std::fs::write(&rtl_path, &rtl_css)?;
                            tracing::info!("RTL CSS: {}", crate::display_path(&rtl_path));
                        }
                    }
                } else {
                    // CSS extraction from JS: if this JS module has extracted CSS, write it as a separate .css file
                    if let Some(ref extracted_css) = cached.extracted_css {
                        let css_hash = blake3::hash(extracted_css.as_bytes());
                        let css_hash_hex = &css_hash.to_hex()[..8];
                        let css_stem = out_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("index");
                        let css_name = format!("{}.{}.css", css_stem, css_hash_hex);
                        let css_out_path = out_path.with_file_name(css_name);
                        let css_rel = crate::normalize_path(
                            css_out_path.strip_prefix(out_dir).unwrap_or(&css_out_path),
                        );
                        std::fs::write(&css_out_path, extracted_css)?;
                        css_files.push(css_rel.clone());
                        tracing::info!("Extracted CSS: {}", crate::display_path(&css_out_path));

                        // RTL CSS auto-generation (#107)
                        if crate::rtl::should_generate_rtl(&self.config.css)
                            && let Some(rtl_css) =
                                crate::rtl::generate_rtl_css(extracted_css, &self.config.css)
                        {
                            let rtl_name = format!("{}.{}.rtl.css", css_stem, css_hash_hex);
                            let rtl_out_path = out_path.with_file_name(rtl_name);
                            std::fs::write(&rtl_out_path, &rtl_css)?;
                            tracing::info!("RTL CSS: {}", crate::display_path(&rtl_out_path));
                        }
                    }

                    // Track async vs sync chunks (using pre-computed is_async)
                    if is_async {
                        async_chunks.push(hashed_rel.clone());
                    } else {
                        js_files.push(hashed_rel.clone());
                    }
                }

                // Track manifest entry (using pre-computed original_rel, is_entry, is_async)
                manifest_entries.insert(
                    original_rel.clone(),
                    ManifestEntry {
                        file: hashed_rel.clone(),
                        is_entry,
                        is_css,
                        is_async,
                        imports: if !is_css {
                            cached.dynamic_imports.clone()
                        } else {
                            Vec::new()
                        },
                        css: if is_css {
                            Some(hashed_rel.clone())
                        } else {
                            None
                        },
                    },
                );

                // Track entry chunks for multi-script HTML
                if is_entry {
                    entry_chunks.push((original_rel, hashed_rel));
                }
            }
        }

        // Apply manual chunks configuration
        if !self.config.build.manual_chunks.is_empty() {
            for (chunk_name, module_patterns) in &self.config.build.manual_chunks {
                let mut chunk_modules: Vec<String> = Vec::new();
                // Build a GlobSet from the patterns for this chunk
                let mut glob_builder = globset::GlobSetBuilder::new();
                for pattern in module_patterns {
                    if let Ok(glob) = globset::Glob::new(pattern) {
                        glob_builder.add(glob);
                    }
                }
                let glob_set = glob_builder.build().unwrap_or_default();
                for module in self.modules_sorted() {
                    if let Some(cached) = self.cached_for(module) {
                        let path_str = crate::normalize_path(&module.path);
                        if glob_set.is_match(&path_str)
                            || module_patterns
                                .iter()
                                .any(|pattern| path_str.contains(pattern) || path_str == *pattern)
                        {
                            let rel = self.output_rel_path(&module.path);
                            let out_path = out_dir.join(rel);
                            let hash = blake3::hash(cached.code.as_bytes());
                            let hash_hex = &hash.to_hex()[..8];
                            let stem = out_path
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or("index");
                            let hashed_name = format!("{}.{}.js", stem, hash_hex);
                            let p = out_path.with_file_name(hashed_name);
                            let hashed_rel =
                                crate::normalize_path(p.strip_prefix(out_dir).unwrap_or(&p));
                            chunk_modules.push(hashed_rel);
                        }
                    }
                }
                if !chunk_modules.is_empty() {
                    tracing::info!(
                        "Manual chunk '{}': {} modules",
                        chunk_name,
                        chunk_modules.len()
                    );
                }
            }
        }

        // Generate manifest.json with entry-to-chunk mapping
        let manifest_json = serde_json::to_string_pretty(&manifest_entries)?;
        std::fs::write(out_dir.join("manifest.json"), manifest_json)?;

        // Write i18n translation catalog (#13) if i18n extraction is enabled and keys were extracted
        if self.config.i18n.enabled && self.config.i18n.extract && !self.i18n_catalog.is_empty() {
            let catalog_path = out_dir.join("i18n-catalog.json");
            std::fs::write(&catalog_path, self.i18n_catalog.to_json())?;
            info!(
                "i18n: extracted {} translation keys → {}",
                self.i18n_catalog.len(),
                crate::display_path(&catalog_path)
            );
        }

        // Generate index.html with CSS links, module preloads, and multi-script entry
        let css_links: String = css_files
            .iter()
            .map(|css| {
                format!(
                    r#"    <link rel="stylesheet" href="{}" />"#,
                    self.config.asset_url(css)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Font subsetting — generate @font-face CSS and preload tags
        let mut font_preload_tags: Vec<String> = Vec::new();
        if self.config.build.font_subsetting {
            let fonts_dir = self.config.root.join("fonts");
            if fonts_dir.exists() {
                let font_config = crate::fonts::FontSubsetConfig::default();
                match crate::fonts::optimize_fonts(&fonts_dir, &font_config) {
                    Ok(subsets) => {
                        if !subsets.is_empty() {
                            let font_css = crate::fonts::generate_subset_css(&subsets);
                            font_preload_tags =
                                crate::fonts::generate_subset_preload_tags(&subsets);
                            // Write font CSS to a file
                            let font_css_hash = blake3::hash(font_css.as_bytes());
                            let font_css_hash_hex = &font_css_hash.to_hex()[..8];
                            let font_css_name = format!("fonts.{}.css", font_css_hash_hex);
                            let font_css_path = out_dir.join(&font_css_name);
                            std::fs::write(&font_css_path, &font_css)?;
                            css_files.push(font_css_name.clone());
                            tracing::info!("Font subsetting: generated {} subsets", subsets.len());
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Font subsetting failed: {}", e);
                    }
                }
            }
        }

        // SVG sprite generation — collect all SVG files and generate a sprite sheet
        if self.config.build.svg_sprite {
            let mut svg_entries: Vec<crate::svg::SvgSpriteEntry> = Vec::new();
            for module in self.modules_sorted() {
                if crate::svg::is_svg(&module.path)
                    && let Some(cached) = self.cached_for(module)
                {
                    let stem = module
                        .path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("icon");
                    svg_entries.push(crate::svg::SvgSpriteEntry {
                        id: stem.to_string(),
                        svg: cached.code.clone(),
                    });
                }
            }
            if !svg_entries.is_empty() {
                let sprite = crate::svg::generate_sprite(&svg_entries);
                let sprite_hash = blake3::hash(sprite.as_bytes());
                let sprite_hash_hex = &sprite_hash.to_hex()[..8];
                let sprite_name = format!("sprite.{}.svg", sprite_hash_hex);
                let sprite_path = out_dir.join(&sprite_name);
                std::fs::write(&sprite_path, &sprite)?;
                tracing::info!("SVG sprite: generated with {} icons", svg_entries.len());
            }
        }

        // Module preload directives — strategy-based (#52)
        // "eager": preload all entry + async chunks
        // "lazy":  only preload entry chunks (default), async chunks loaded on demand
        // "manual": skip auto-generation, user controls via HTML
        let module_preloads: String = match self.config.build.preload_strategy.as_str() {
            "manual" => String::new(),
            "eager" => {
                let mut chunks_to_preload: Vec<&String> = Vec::new();
                // Preload entry chunks first
                for (_, hashed) in &entry_chunks {
                    chunks_to_preload.push(hashed);
                }
                // Then async chunks
                for chunk in &async_chunks {
                    if !chunks_to_preload.contains(&chunk) {
                        chunks_to_preload.push(chunk);
                    }
                }
                if self.config.build.module_preload {
                    chunks_to_preload
                        .iter()
                        .map(|chunk| {
                            format!(
                                r#"    <link rel="modulepreload" href="{}" />"#,
                                self.config.asset_url(chunk)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            }
            _ => {
                // "lazy" (default) — only preload entry chunks
                if self.config.build.module_preload {
                    entry_chunks
                        .iter()
                        .map(|(_, hashed)| {
                            format!(
                                r#"    <link rel="modulepreload" href="{}" />"#,
                                self.config.asset_url(hashed)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                } else {
                    String::new()
                }
            }
        };

        // Preload directives for critical assets (fonts, images)
        let preload_links: String = if self.config.build.preload {
            let mut links: Vec<String> = Vec::new();
            // Preload first CSS file
            if let Some(first_css) = css_files.iter().find(|css| css.ends_with(".css")) {
                links.push(format!(
                    r#"    <link rel="preload" href="{}" as="style" />"#,
                    self.config.asset_url(first_css)
                ));
            }
            // Font preload tags from subsetting
            for tag in &font_preload_tags {
                links.push(format!(r#"    {}"#, tag));
            }
            links.join("\n")
        } else {
            // Even if preload is off, include font preloads if subsetting is on
            if !font_preload_tags.is_empty() {
                font_preload_tags
                    .iter()
                    .map(|t| format!(r#"    {}"#, t))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                String::new()
            }
        };

        // Prefetch directives for non-critical assets
        let prefetch_links: String = if self.config.build.prefetch {
            async_chunks
                .iter()
                .map(|chunk| {
                    format!(
                        r#"    <link rel="prefetch" href="{}" />"#,
                        self.config.asset_url(chunk)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };

        // Build script tags — support multiple entry points
        let script_tags: String = if entry_chunks.is_empty() {
            // Fallback: use first entry from config (guard against empty entries)
            if entries.is_empty() {
                tracing::warn!("No entry points configured — skipping script tag generation");
                String::new()
            } else {
                let entry = &entries[0];
                let entry_js = entry
                    .replace(".tsx", ".js")
                    .replace(".ts", ".js")
                    .replace(".jsx", ".js");
                let entry_hashed = manifest_entries
                    .values()
                    .find(|m| m.is_entry)
                    .map(|m| m.file.clone())
                    .unwrap_or(entry_js);
                format!(
                    r#"    <script type="module" src="{}"></script>"#,
                    self.config.asset_url(&entry_hashed)
                )
            }
        } else {
            entry_chunks
                .iter()
                .map(|(_, hashed)| {
                    format!(
                        r#"    <script type="module" src="{}"></script>"#,
                        self.config.asset_url(hashed)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Use project's index.html as template if it exists, otherwise generate default
        let project_html_path = self.config.root.join("index.html");
        let html = if project_html_path.exists() {
            if let Ok(template) = std::fs::read_to_string(&project_html_path) {
                // Strip the dev-mode entry `<script src="...">` tag(s) before
                // injecting the built, hashed chunk's — see the identical
                // fix's doc comment on `remove_entry_script_tags` above for
                // why this is necessary (PRODUCTION-READINESS-100.md: found
                // via an actual end-to-end `pledge build` smoke test).
                let template = remove_entry_script_tags(&template, &entries);
                // Inject CSS links and script tags into the custom template
                let mut html = template;
                let has_head = html.contains("</head>");
                let has_body = html.contains("</body>");

                // Inject CSS links before </head>
                if !css_links.is_empty() {
                    let injection = format!("{}\n", css_links);
                    if let Some(pos) = html.rfind("</head>") {
                        html.insert_str(pos, &injection);
                    } else if !has_head {
                        html.push_str(&injection);
                    }
                }

                // Inject module preloads before </head>
                if !module_preloads.is_empty() {
                    let injection = format!("{}\n", module_preloads);
                    if let Some(pos) = html.rfind("</head>") {
                        html.insert_str(pos, &injection);
                    } else if !has_head {
                        html.push_str(&injection);
                    }
                }

                // Inject preload links before </head>
                if !preload_links.is_empty() {
                    let injection = format!("{}\n", preload_links);
                    if let Some(pos) = html.rfind("</head>") {
                        html.insert_str(pos, &injection);
                    } else if !has_head {
                        html.push_str(&injection);
                    }
                }

                // Inject prefetch links before </head>
                if !prefetch_links.is_empty() {
                    let injection = format!("{}\n", prefetch_links);
                    if let Some(pos) = html.rfind("</head>") {
                        html.insert_str(pos, &injection);
                    } else if !has_head {
                        html.push_str(&injection);
                    }
                }

                // Inject script tags before </body>
                if !script_tags.is_empty() {
                    let injection = format!("{}\n", script_tags);
                    if let Some(pos) = html.rfind("</body>") {
                        html.insert_str(pos, &injection);
                    } else if !has_body {
                        html.push_str(&injection);
                    }
                }

                html
            } else {
                // Fallback to generated HTML
                format!(
                    r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>.pledge</title>
{}
{}
{}
{}
</head>
<body>
    <div id="root"></div>
{}
</body>
</html>"#,
                    css_links, module_preloads, preload_links, prefetch_links, script_tags
                )
            }
        } else {
            // No custom index.html — generate default
            format!(
                r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>.pledge</title>
    <style>* {{ margin: 0; padding: 0; box-sizing: border-box; }} body {{ background: #0a0a0a; }}</style>
{}
{}
{}
{}
</head>
<body>
    <div id="root"></div>
{}
</body>
</html>"#,
                css_links, module_preloads, preload_links, prefetch_links, script_tags
            )
        };
        let html = match runner {
            Some(r) => r.transform_index_html(&html, "index.html")?,
            None => html,
        };
        std::fs::write(out_dir.join("index.html"), html)?;

        // Build output verification (#53)
        if self.config.build.verify_output {
            self.verify_build_output(out_dir, &manifest_entries)?;
        }

        Ok(())
    }

    /// Verify build output integrity (#53).
    /// Checks that all manifest entries exist on disk, no broken import references,
    /// and all referenced assets are present in the output directory.
    fn verify_build_output(
        &self,
        out_dir: &std::path::Path,
        manifest_entries: &std::collections::BTreeMap<String, ManifestEntry>,
    ) -> Result<()> {
        tracing::info!("Verifying build output...");

        let mut errors: Vec<String> = Vec::new();
        let mut checked = 0;

        for (original, entry) in manifest_entries {
            let file_path = out_dir.join(&entry.file);

            // Check 1: file exists
            if !file_path.exists() {
                errors.push(format!(
                    "Missing output file: {} (referenced by {})",
                    entry.file, original
                ));
                continue;
            }

            // Check 2: file is not empty
            let metadata = std::fs::metadata(&file_path)?;
            if metadata.len() == 0 {
                errors.push(format!("Empty output file: {}", entry.file));
                continue;
            }

            // Check 3: for JS files, verify import references resolve
            if !entry.is_css
                && !entry.is_async
                && let Ok(content) = std::fs::read_to_string(&file_path)
            {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if (trimmed.starts_with("import ") || trimmed.starts_with("export "))
                        && let Some(from_pos) = trimmed.find(" from \"")
                    {
                        let rest = &trimmed[from_pos + 7..];
                        if let Some(end) = rest.find('"') {
                            let import_path = &rest[..end];
                            if import_path.starts_with("./") || import_path.starts_with("../") {
                                let resolved = file_path
                                    .parent()
                                    .map(|p| {
                                        p.join(
                                            import_path
                                                .replace(".js", "")
                                                .replace(".ts", "")
                                                .replace(".tsx", ""),
                                        )
                                    })
                                    .unwrap_or_default();
                                let found = [".js", ".mjs", ".css", ".json"].iter().any(|ext| {
                                    resolved
                                        .with_extension(ext.trim_start_matches('.'))
                                        .exists()
                                });
                                if !found && !resolved.exists() {
                                    errors.push(format!(
                                                "Broken import in {}: \"{}\" does not resolve to any output file",
                                                entry.file, import_path
                                            ));
                                }
                            }
                        }
                    }
                }
            }

            // Check 4: for CSS files referenced in manifest, verify they exist
            if let Some(ref css) = entry.css {
                let css_path = out_dir.join(css);
                if !css_path.exists() {
                    errors.push(format!(
                        "Missing CSS file: {} (referenced by {})",
                        css, original
                    ));
                }
            }

            checked += 1;
        }

        // Check 5: verify index.html exists
        if !out_dir.join("index.html").exists() {
            errors.push("Missing index.html in output directory".to_string());
        }

        // Check 6: verify manifest.json exists
        if !out_dir.join("manifest.json").exists() {
            errors.push("Missing manifest.json in output directory".to_string());
        }

        if errors.is_empty() {
            tracing::info!(
                "Build verification passed: {} files checked, all OK",
                checked
            );
        } else {
            tracing::error!("Build verification failed with {} error(s):", errors.len());
            for err in &errors {
                tracing::error!("  ✗ {}", err);
            }
            bail!(
                "Build output verification failed: {} error(s)",
                errors.len()
            );
        }

        Ok(())
    }

    /// Emit a single-file bundle — concatenate all modules into one ESM file.
    /// All imports are inlined, no separate chunks.
    pub fn emit_single_file(&self) -> Result<()> {
        self.emit_single_file_hooks(None)
    }

    /// [`emit_single_file`](Self::emit_single_file) with plugin hooks
    /// (`renderChunk(code, "index.js", "entry")`, `transformIndexHtml`).
    pub fn emit_single_file_hooks(&self, hooks: Option<&dyn PluginHooks>) -> Result<()> {
        let runner = hooks.map(HookRunner::new);
        let runner = runner.as_ref();
        let out_dir = &self.config.out_dir;
        if out_dir == &self.config.root {
            anyhow::bail!("Output directory cannot be the same as project root");
        }
        // Acquire the output lock BEFORE wiping out_dir (see emit_with_chunks).
        ensure_safe_out_dir(out_dir, &self.config.root, &self.config.entry)?;
        let _output_lock = OutputLock::acquire(out_dir)?;
        if out_dir.exists() {
            // Safety: refuse to delete root, home, or empty paths
            let canonical = out_dir.canonicalize().unwrap_or(out_dir.to_path_buf());
            if canonical == std::path::Path::new("/") || canonical.parent().is_none() {
                bail!(
                    "Refusing to delete unsafe output directory: {}",
                    crate::display_path(&canonical)
                );
            }
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;

        let mut bundle = String::new();
        let mut css_bundle = String::new();

        // Collect all module codes in dependency order
        let mut visited = std::collections::HashSet::new();
        let mut entry_ids = self.entry_ids();
        if entry_ids.is_empty() {
            // No recorded entries: fall back to the lowest module id (ids are
            // assigned in deterministic discovery order) rather than an
            // arbitrary HashMap element.
            entry_ids.extend(self.modules.keys().min().copied());
        }
        for entry in entry_ids {
            self.collect_module_code(entry, &mut visited, &mut bundle, &mut css_bundle);
        }

        if let Some(r) = runner
            && let Some(res) = r.render_chunk(&bundle, "index.js", "entry")?
        {
            bundle = res.code;
        }

        // Write the single JS bundle
        let js_hash = blake3::hash(bundle.as_bytes());
        let js_hash_hex = &js_hash.to_hex()[..8];
        let js_filename = format!("index.{}.js", js_hash_hex);
        std::fs::write(out_dir.join(&js_filename), &bundle)?;

        // Write CSS bundle if any
        let css_filename = if !css_bundle.is_empty() {
            let css_hash = blake3::hash(css_bundle.as_bytes());
            let css_hash_hex = &css_hash.to_hex()[..8];
            let css_fn = format!("index.{}.css", css_hash_hex);
            std::fs::write(out_dir.join(&css_fn), &css_bundle)?;
            Some(css_fn)
        } else {
            None
        };

        // Generate HTML
        let css_link = css_filename
            .map(|f| format!(r#"    <link rel="stylesheet" href="/{}" />"#, f))
            .unwrap_or_default();

        let html = format!(
            r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>.pledge</title>
    <style>* {{ margin: 0; padding: 0; box-sizing: border-box; }} body {{ background: #0a0a0a; }}</style>
{}
</head>
<body>
    <div id="root"></div>
    <script type="module" src="/{}"></script>
</body>
</html>"#,
            css_link, js_filename
        );
        let html = match runner {
            Some(r) => r.transform_index_html(&html, "index.html")?,
            None => html,
        };
        std::fs::write(out_dir.join("index.html"), html)?;

        // Write i18n translation catalog (#13) if i18n extraction is enabled and keys were extracted
        if self.config.i18n.enabled && self.config.i18n.extract && !self.i18n_catalog.is_empty() {
            let catalog_path = out_dir.join("i18n-catalog.json");
            std::fs::write(&catalog_path, self.i18n_catalog.to_json())?;
            info!(
                "i18n: extracted {} translation keys → {}",
                self.i18n_catalog.len(),
                crate::display_path(&catalog_path)
            );
        }

        Ok(())
    }

    /// Collect module code for a single-file bundle: `module_id` and all of
    /// its static dependencies, dependencies first (post-order), each module
    /// once. Iterative so deep import chains cannot overflow the stack, and
    /// cycle-safe via `visited`.
    pub fn collect_module_code(
        &self,
        module_id: ModuleId,
        visited: &mut std::collections::HashSet<ModuleId>,
        bundle: &mut String,
        css_bundle: &mut String,
    ) {
        // (id, children_already_pushed)
        let mut stack: Vec<(ModuleId, bool)> = vec![(module_id, false)];
        while let Some((id, expanded)) = stack.pop() {
            if expanded {
                self.append_module_code(id, bundle, css_bundle);
                continue;
            }
            if !visited.insert(id) {
                continue;
            }
            stack.push((id, true));
            if let Some(node) = self.module_graph.modules.get(&id) {
                // Reverse so the first import is processed (and emitted) first.
                for dep in node.dependencies.iter().rev() {
                    if !visited.contains(dep) {
                        stack.push((*dep, false));
                    }
                }
            }
        }
    }

    fn append_module_code(
        &self,
        module_id: ModuleId,
        bundle: &mut String,
        css_bundle: &mut String,
    ) {
        if let Some(module) = self.modules.get(&module_id) {
            // Add this module's code
            if let Some(cached) = self.cached_for(module) {
                if cached.is_css {
                    css_bundle.push_str(&cached.code);
                    css_bundle.push('\n');
                } else {
                    let rel = module
                        .path
                        .strip_prefix(&self.config.root)
                        .unwrap_or(&module.path);
                    bundle.push_str(&format!("// === {} ===\n", crate::display_path(&rel)));
                    bundle.push_str(&cached.code);
                    bundle.push('\n');
                }
            }
        }
    }

    /// Invalidate modules that depend on a changed file
    pub fn invalidate(&mut self, changed_path: &PathBuf) -> Vec<ModuleId> {
        if let Some(&id) = self.path_to_id.get(changed_path) {
            let dependents = self
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_all_dependents(id);

            // Remove from function cache
            if let Some(module) = self.modules.get(&id) {
                self.function_cache
                    .remove(&module_cache_key(module.content_hash, &module.path));
            }

            // Also invalidate dependents
            for dep_id in &dependents {
                if let Some(module) = self.modules.get(dep_id) {
                    self.function_cache
                        .remove(&module_cache_key(module.content_hash, &module.path));
                }
            }

            let mut all = vec![id];
            all.extend(dependents);
            all
        } else {
            warn!("Invalidation: module not found: {:?}", changed_path);
            vec![]
        }
    }
}

/// Outcome of resolving an import.
#[derive(Debug, Clone)]
enum Resolution {
    /// A module (a real file, or a virtual id a plugin `load` hook provides).
    File(PathBuf),
    /// Left as-is in the output (plugin `resolveId` returned `external`).
    External,
}

/// Key of the in-memory function cache. It mixes the path and module kind
/// into the content hash: keyed by content alone, two files with identical
/// text at different paths (or `x.ts` vs `x.tsx`, which compile differently)
/// shared one entry — and therefore one output.
pub fn module_cache_key(content_hash: u64, path: &Path) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(&content_hash.to_le_bytes());
    h.update(crate::normalize_path(path).as_bytes());
    h.update(&[0]);
    h.update(format!("{:?}", ModuleKind::from_path(path)).as_bytes());
    u64::from_le_bytes(h.finalize().as_bytes()[0..8].try_into().unwrap())
}

/// Modules of a previous build's graph whose cached output can be reused:
/// everything except modules whose content changed and the modules that
/// (transitively) depend on them. Identity is the module *path* — ids are
/// per-build and shift when discovery order changes — so `current` supplies
/// `(path, content_hash)` for modules known in this build. Returns
/// `(path, previous content hash)` pairs.
fn unchanged_prev_modules(
    prev: &SerializableModuleGraph,
    current: &[(PathBuf, u64)],
) -> Vec<(PathBuf, u64)> {
    let prev_by_path: HashMap<&Path, &crate::module_graph::ModuleNode> = prev
        .modules
        .values()
        .map(|n| (n.path.as_path(), n))
        .collect();
    let mut changed: HashSet<ModuleId> = HashSet::new();
    for (path, hash) in current {
        if let Some(node) = prev_by_path.get(path.as_path())
            && node.content_hash != *hash
        {
            changed.insert(node.id);
        }
    }
    let affected = prev.compute_affected(&changed);
    let mut out: Vec<(PathBuf, u64)> = prev
        .modules
        .iter()
        .filter(|(id, _)| !affected.contains(id))
        .map(|(_, n)| (n.path.clone(), n.content_hash))
        .collect();
    out.sort();
    out
}

fn sanitize_path_component(c: &str) -> String {
    c.chars()
        .map(|ch| match ch {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            other => other,
        })
        .collect()
}

/// Make a chunk id safe to use as a file-name stem: only `[A-Za-z0-9._-]`
/// survive (path separators, `..`, `:` and friends become `_`), no leading
/// dot, never empty.
pub fn sanitize_chunk_id(id: &str) -> String {
    let mapped: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `..` can only survive as dots; neutralise them so an id can never
    // read as a parent-directory reference.
    let mapped = mapped.replace("..", "_");
    let trimmed = mapped.trim_start_matches('.');
    if trimmed.is_empty() {
        "chunk".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Drop a trailing `//# sourceMappingURL=...` line from module code.
fn strip_source_mapping_url(code: &str) -> &str {
    let trimmed = code.trim_end();
    match trimmed.rfind('\n') {
        Some(nl) if trimmed[nl + 1..].starts_with("//# sourceMappingURL=") => &code[..nl],
        None if trimmed.starts_with("//# sourceMappingURL=") => "",
        _ => code,
    }
}

/// Merge per-module source maps into one map for a concatenated chunk.
///
/// `sections` holds `(line, map)` where `line` is the 0-based line of the
/// chunk at which that module's code starts (columns start at 0). The result
/// is a standard *indexed* source map (`sections`), which every consumer
/// (browsers, `source-map`) understands, so no VLQ re-encoding is needed and
/// mappings stay exact. A single map starting at line 0 is returned as a
/// plain map. Unparseable maps are skipped.
fn merge_chunk_source_maps(file: &str, sections: &[(usize, String)]) -> Option<String> {
    let mut parsed: Vec<(usize, serde_json::Value)> = sections
        .iter()
        .filter_map(|(line, json)| Some((*line, serde_json::from_str(json).ok()?)))
        .collect();
    match parsed.len() {
        0 => None,
        1 if parsed[0].0 == 0 => {
            let mut map = parsed.remove(0).1;
            map["file"] = serde_json::Value::String(file.to_string());
            Some(map.to_string())
        }
        _ => {
            let sections: Vec<serde_json::Value> = parsed
                .into_iter()
                .map(|(line, map)| {
                    serde_json::json!({ "offset": { "line": line, "column": 0 }, "map": map })
                })
                .collect();
            Some(
                serde_json::json!({ "version": 3, "file": file, "sections": sections }).to_string(),
            )
        }
    }
}

/// Write output file to disk.
///
/// This previously used `MAP_SHARED` mmap for large files, but that path did
/// not verify that dirty pages were flushed before the mapping was torn down
/// (`munmap` does flush, but a `msync(MS_SYNC)` check was missing and error
/// handling was incomplete). The simpler, safer approach — buffered
/// `write_all` followed by `sync_all` — is used for all sizes. For typical
/// bundle output sizes the kernel write-back cache makes this plenty fast,
/// and it guarantees durability via `sync_all`.
fn write_output_file(path: &std::path::Path, content: &str) -> Result<()> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// Extract the module specifier from an import statement
/// e.g., "import React from 'react'" → "react"
/// Refuse to wipe an output directory that would destroy project sources.
///
/// `out_dir == root` is not enough: `outDir: "."`, `".."`, an absolute path to
/// the root spelled differently, or a source folder such as `src` all pass a
/// literal comparison yet get recursively deleted by `emit*`. Only checked
/// when `out_dir` exists (nothing is deleted otherwise), so both sides can be
/// canonicalized consistently.
fn ensure_safe_out_dir(out_dir: &Path, root: &Path, entries: &[String]) -> Result<()> {
    if !out_dir.exists() {
        return Ok(());
    }
    let Ok(out) = out_dir.canonicalize() else {
        return Ok(());
    };
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if root_canon.starts_with(&out) {
        bail!(
            "Refusing to delete output directory {}: it is the project root or one of its parents",
            crate::display_path(&out)
        );
    }
    for entry in entries {
        if let Ok(e) = root.join(entry).canonicalize()
            && e.starts_with(&out)
        {
            bail!(
                "Refusing to delete output directory {}: it contains the entry point {}",
                crate::display_path(&out),
                entry
            );
        }
    }
    Ok(())
}

/// Resolve `path` to a file the way bundlers do: the exact file, then the
/// path with each configured extension *appended* (so dotted names such as
/// `./user.service` find `user.service.ts`), then the TypeScript-style
/// `./x.js` → `x.ts`/`x.tsx` mapping, then `<dir>/index<ext>`.
fn resolve_file_like(path: &Path, extensions: &[String]) -> Option<PathBuf> {
    fn dotted(ext: &str) -> String {
        if ext.starts_with('.') {
            ext.to_string()
        } else {
            format!(".{ext}")
        }
    }
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.file_name().is_some() {
        for ext in extensions {
            let mut os = path.as_os_str().to_owned();
            os.push(dotted(ext));
            let candidate = PathBuf::from(os);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        let alts: &[&str] = match path.extension().and_then(|e| e.to_str()) {
            Some("js") => &["ts", "tsx"],
            Some("jsx") => &["tsx"],
            Some("mjs") => &["mts"],
            Some("cjs") => &["cts"],
            _ => &[],
        };
        for alt in alts {
            let candidate = path.with_extension(alt);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if path.is_dir() {
        for ext in extensions {
            let index = path.join(format!("index{}", dotted(ext)));
            if index.is_file() {
                return Some(index);
            }
        }
    }
    None
}

/// Resolve a package.json `exports` entry for `key` (`"."` or `"./sub"`).
/// The matching logic is shared with the standalone resolver crate - see
/// [`crate::package_map`].
fn resolve_package_exports(
    exports: &serde_json::Value,
    key: &str,
    conditions: &[String],
) -> Option<String> {
    crate::package_map::resolve_exports_entry(exports, key, conditions)
}

fn extract_module_specifier(source: &str, offset: usize) -> Option<String> {
    // `offset` comes from a byte scan of the raw source; the lossy UTF-8 string
    // can differ in length, so never slice off a char boundary.
    let rest = source.get(offset..)?;
    let before = source.get(..offset)?;
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
        let mut at_boundary = before.trim().is_empty();
        for ch in before.chars().rev() {
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
        // Clause form (import {..}/import x/export {..}): require a `from`
        // token before the statement ends. `;` ends the statement; `<`/`>`
        // can't appear in an import/export clause, so they also bound it —
        // this stops the scan from wandering into JSX text.
        let mut has_from = false;
        let mut cur = String::new();
        for ch in after.chars() {
            match ch {
                ';' | '<' | '>' => break,
                // A quote right after `from` is the specifier (`from"x"`);
                // anywhere else it starts a string *value*, so this is a
                // declaration (`export const s = 'a from b'`), not a clause —
                // scanning on would find the word `from` inside the string.
                '\'' | '"' | '`' => {
                    has_from = cur == "from";
                    cur.clear();
                    break;
                }
                // Declarations / calls never appear inside an import/export
                // clause (`export const x = ..`, `export default function from()`).
                '=' | '(' => {
                    cur.clear();
                    break;
                }
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

    let source = after;
    // Find the first string literal (single or double quoted)
    let bytes = source.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            let start = i + 1;
            let mut end = start;

            while end < bytes.len() && bytes[end] != quote {
                end += 1;
            }

            if end < bytes.len() {
                return Some(source[start..end].to_string());
            }
        }
        i += 1;

        // Don't scan too far — just look for the first string
        if i > 200 {
            break;
        }
    }

    None
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_module_specifier() {
        assert_eq!(
            extract_module_specifier("import React from 'react'", 0),
            Some("react".to_string())
        );
        assert_eq!(
            extract_module_specifier("import { foo } from \"./bar\"", 0),
            Some("./bar".to_string())
        );
        assert_eq!(
            extract_module_specifier("import('./lazy')", 0),
            Some("./lazy".to_string())
        );
    }

    #[test]
    fn extract_module_specifier_rejects_false_positive_matches() {
        // find_imports is a raw substring scan — `import` inside JSX text or
        // string literals must not produce bogus deps. Regression: a template
        // page containing "import maps" prose extracted `className="card"` as
        // a dependency and failed the whole build.
        let jsx_text =
            "<p>Debug meta tags, import maps, and script injection.</p>\n<div className=\"card\">";
        let off = jsx_text.find("import").unwrap();
        assert_eq!(extract_module_specifier(jsx_text, off), None);

        // `import` inside a string literal — preceding char is identifier text.
        let in_string = "const s = 'auto-generates import maps for bare specifiers.';";
        let off = in_string.find("import").unwrap();
        assert_eq!(extract_module_specifier(in_string, off), None);

        // `export const` has no dependency despite a quoted literal.
        let export_stmt = "export const x = 'value';";
        assert_eq!(extract_module_specifier(export_stmt, 0), None);

        // import.meta is not a dependency.
        assert_eq!(extract_module_specifier("import.meta.env", 0), None);

        // ASI: newline-separated statement is a valid boundary.
        let asi = "const x = 1\nimport y from 'z';";
        let off = asi.find("import").unwrap();
        assert_eq!(extract_module_specifier(asi, off), Some("z".to_string()));

        // Dynamic import mid-expression doesn't need a statement boundary.
        let dyn_import = "const p = import('./lazy');";
        let off = dyn_import.find("import").unwrap();
        assert_eq!(
            extract_module_specifier(dyn_import, off),
            Some("./lazy".to_string())
        );

        // Multi-line clause import.
        let multi = "import {\n  a,\n  b,\n} from 'pkg';";
        assert_eq!(extract_module_specifier(multi, 0), Some("pkg".to_string()));
    }

    // Regression test for a real bug found via an end-to-end `pledge build`
    // smoke test (2026-09-15, see PRODUCTION-READINESS-100.md): production
    // HTML generation injected the built, hashed entry chunk's <script> tag
    // but never removed the template's original dev-mode entry <script>
    // (which points at the raw, untransformed source file) — producing a
    // page with two script tags, one of which 404s / fails MIME-type
    // checking in a real browser since production output never serves the
    // raw source tree.
    #[test]
    fn remove_entry_script_tags_strips_the_dev_mode_entry_reference() {
        let template = r#"<!DOCTYPE html>
<html>
<head><title>app</title></head>
<body>
    <div id="root"></div>
    <script type="module" src="/src/index.tsx"></script>
</body>
</html>"#;
        let entries = vec!["src/index.tsx".to_string()];
        let result = remove_entry_script_tags(template, &entries);
        assert!(
            !result.contains("src/index.tsx"),
            "the raw source entry script tag should be fully removed, got:\n{result}"
        );
        assert!(result.contains("<div id=\"root\"></div>"));
    }

    #[test]
    fn remove_entry_script_tags_matches_with_and_without_leading_slash() {
        let template = r#"<body><script type="module" src="src/index.tsx"></script></body>"#;
        let entries = vec!["/src/index.tsx".to_string()];
        let result = remove_entry_script_tags(template, &entries);
        assert!(!result.contains("script"));
    }

    #[test]
    fn remove_entry_script_tags_leaves_unrelated_scripts_alone() {
        let template = r#"<body>
    <script type="module" src="/src/index.tsx"></script>
    <script src="/analytics.js"></script>
</body>"#;
        let entries = vec!["src/index.tsx".to_string()];
        let result = remove_entry_script_tags(template, &entries);
        assert!(!result.contains("index.tsx"));
        assert!(
            result.contains("/analytics.js"),
            "unrelated script tags must not be touched, got:\n{result}"
        );
    }

    #[test]
    fn extract_module_specifier_ignores_from_inside_string_values() {
        // The word `from` inside a string literal used to make a plain
        // declaration look like `export ... from '<string>'`.
        let src = "export const greeting = 'hello from plugin';";
        assert_eq!(extract_module_specifier(src, 0), None);
        let src = "export default function from() {}";
        assert_eq!(extract_module_specifier(src, 0), None);
        // Real clauses still work, including a spaceless `from\"x\"`.
        let src = "export { a } from 'x';";
        assert_eq!(extract_module_specifier(src, 0).as_deref(), Some("x"));
        let src = "import a from\"y\";";
        assert_eq!(extract_module_specifier(src, 0).as_deref(), Some("y"));
        let src = "export * from './z';";
        assert_eq!(extract_module_specifier(src, 0).as_deref(), Some("./z"));
    }

    #[test]
    fn extract_module_specifier_does_not_panic_off_char_boundary() {
        // Offsets come from a byte scan of the raw source; after lossy UTF-8
        // conversion they can land inside a multibyte char.
        assert_eq!(extract_module_specifier("é import x from 'y'", 1), None);
        assert_eq!(extract_module_specifier("abc", 99), None);
    }

    fn resolver_engine(root: &Path) -> BuildEngine {
        let mut cfg = PledgeConfig::default();
        cfg.root = root.to_path_buf();
        cfg.cache.enabled = false;
        BuildEngine::new(Arc::new(cfg))
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn resolve_relative_import_with_dotted_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "src/main.ts", "");
        write(root, "src/user.service.ts", "export {}");
        let eng = resolver_engine(root);
        let importer = root.join("src/main.ts");
        let got = eng.resolve("./user.service", Some(&importer)).unwrap();
        assert_eq!(got, root.join("src/user.service.ts"));
    }

    #[test]
    fn resolve_js_specifier_maps_to_ts_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "src/main.ts", "");
        write(root, "src/util.ts", "export {}");
        let eng = resolver_engine(root);
        let importer = root.join("src/main.ts");
        let got = eng.resolve("./util.js", Some(&importer)).unwrap();
        assert_eq!(got, root.join("src/util.ts"));
    }

    #[test]
    fn resolve_alias_from_resolve_alias_is_relative_to_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(root, "src/components/button.tsx", "export {}");
        let mut cfg = PledgeConfig::default();
        cfg.root = root.to_path_buf();
        cfg.cache.enabled = false;
        cfg.resolve_alias = vec![crate::config::PathAlias {
            from: "@".to_string(),
            to: "./src".to_string(),
        }];
        let eng = BuildEngine::new(Arc::new(cfg));
        let got = eng.resolve("@/components/button", None).unwrap();
        assert_eq!(got, root.join("src/components/button.tsx"));
    }

    #[test]
    fn resolve_exports_prefers_import_over_require_and_handles_nesting() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            root,
            "node_modules/dual/package.json",
            r#"{"name":"dual","exports":{".":{"types":"./t.d.ts","require":"./cjs.js","default":"./esm.js"},"./feat/*":{"import":{"types":"./x.d.ts","default":"./lib/*.mjs"}}}}"#,
        );
        write(root, "node_modules/dual/cjs.js", "");
        write(root, "node_modules/dual/esm.js", "");
        write(root, "node_modules/dual/lib/a.mjs", "");
        let eng = resolver_engine(root);
        assert_eq!(
            eng.resolve("dual", None).unwrap(),
            root.join("node_modules/dual/esm.js"),
            "must not pick the `require` condition for an ESM bundle"
        );
        assert_eq!(
            eng.resolve("dual/feat/a", None).unwrap(),
            root.join("node_modules/dual/lib/a.mjs")
        );
    }

    #[test]
    fn emit_refuses_to_wipe_a_source_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root, "src/index.tsx", "export {}");
        let mut cfg = PledgeConfig::default();
        cfg.root = root.clone();
        cfg.cache.enabled = false;
        cfg.entry = vec!["src/index.tsx".to_string()];
        cfg.out_dir = root.join("src");
        let eng = BuildEngine::new(Arc::new(cfg));
        assert!(eng.emit().is_err());
        assert!(root.join("src/index.tsx").exists(), "sources were deleted");

        // Root spelled differently from the literal `root` still refused.
        let mut cfg = PledgeConfig::default();
        cfg.root = root.clone();
        cfg.cache.enabled = false;
        cfg.out_dir = root.join("src").join("..");
        let eng = BuildEngine::new(Arc::new(cfg));
        assert!(eng.emit().is_err());
        assert!(root.join("src/index.tsx").exists(), "project was deleted");
    }

    fn cached(code: &str) -> CachedOutput {
        CachedOutput {
            code: code.to_string(),
            source_map: None,
            deps: vec![],
            is_css: false,
            css_modules: None,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: vec![],
        }
    }

    fn engine_with_modules(
        root: &Path,
        out: &Path,
        entry: &str,
        files: &[(PathBuf, &str)],
    ) -> BuildEngine {
        let mut cfg = PledgeConfig::default();
        cfg.root = root.to_path_buf();
        cfg.cache.enabled = false;
        cfg.entry = vec![entry.to_string()];
        cfg.out_dir = out.to_path_buf();
        let mut eng = BuildEngine::new(Arc::new(cfg));
        for (path, code) in files {
            let id = eng
                .add_module_with_source(path.clone(), code.as_bytes().to_vec(), None)
                .unwrap();
            let hash = eng.modules[&id].content_hash;
            eng.function_cache
                .insert(module_cache_key(hash, path), cached(code));
        }
        eng
    }

    #[test]
    fn collect_module_code_includes_dependencies_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (a, b, c) = (root.join("a.ts"), root.join("b.ts"), root.join("c.ts"));
        let mut eng = engine_with_modules(
            &root,
            &root.join("dist"),
            "a.ts",
            &[(a, "A_CODE"), (b, "B_CODE"), (c, "C_CODE")],
        );
        // a -> b -> c, plus a cycle c -> a
        for id in 0..3u32 {
            let m = eng.modules[&id].clone();
            eng.module_graph
                .add_module(id, m.path.clone(), m.kind, m.content_hash);
        }
        eng.module_graph.add_dependency(0, 1);
        eng.module_graph.add_dependency(1, 2);
        eng.module_graph.add_dependency(2, 0);
        let mut visited = HashSet::new();
        let (mut js, mut css) = (String::new(), String::new());
        eng.collect_module_code(0, &mut visited, &mut js, &mut css);
        let (pa, pb, pc) = (
            js.find("A_CODE").expect("entry missing"),
            js.find("B_CODE").expect("dependency b missing"),
            js.find("C_CODE").expect("transitive dependency c missing"),
        );
        assert!(pc < pb && pb < pa, "dependencies must precede dependents");
    }

    #[test]
    fn emit_keeps_external_modules_inside_out_dir_and_matches_entries_on_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("proj");
        let ext = tmp.path().canonicalize().unwrap().join("hoisted");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&ext).unwrap();
        let out = root.join("dist");
        let eng = engine_with_modules(
            &root,
            &out,
            "index.tsx",
            &[
                (root.join("index.tsx"), "ENTRY"),
                (root.join("my-index.tsx"), "NOT_ENTRY"),
                (ext.join("dep.js"), "EXTERNAL"),
            ],
        );
        eng.emit().unwrap();
        let leaked: Vec<_> = std::fs::read_dir(&ext)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert!(leaked.is_empty(), "wrote outside out_dir: {leaked:?}");
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["index.tsx"]["is_entry"], true);
        assert_eq!(manifest["my-index.tsx"]["is_entry"], false);
    }

    // ─── Plugin hooks wired into the engine ─────────────────────────────

    use crate::plugin_hooks::{
        BuildHook, CodeResult, HtmlResult, HtmlTagSpec, PluginHooks, ResolvedId,
    };
    use std::cell::RefCell;

    /// One fake plugin implementing every per-module hook.
    #[derive(Default)]
    struct TestPlugin {
        log: RefCell<Vec<String>>,
    }

    impl PluginHooks for TestPlugin {
        fn plugin_count(&self) -> usize {
            1
        }
        fn plugin_name(&self, _: usize) -> String {
            "test-plugin".to_string()
        }
        fn supports(&self, _: usize, _: BuildHook) -> bool {
            true
        }
        fn resolve_id(
            &self,
            _: usize,
            source: &str,
            _: Option<&str>,
        ) -> Result<Option<ResolvedId>> {
            self.log.borrow_mut().push(format!("resolve:{source}"));
            Ok(match source {
                "virtual:greeting" => Some(ResolvedId {
                    id: "virtual:greeting".into(),
                    external: false,
                }),
                "cdn-lib" => Some(ResolvedId {
                    id: "cdn-lib".into(),
                    external: true,
                }),
                _ => None,
            })
        }
        fn load(&self, _: usize, id: &str) -> Result<Option<CodeResult>> {
            Ok((id == "virtual:greeting").then(|| CodeResult {
                code: "export const greeting = 'hello from plugin';".into(),
                map: None,
            }))
        }
        fn transform(&self, _: usize, code: &str, id: &str) -> Result<Option<CodeResult>> {
            if code.contains("FAIL_ME") {
                anyhow::bail!("refusing {id}");
            }
            Ok(code.contains("__ANSWER__").then(|| CodeResult {
                code: code.replace("__ANSWER__", "42"),
                map: None,
            }))
        }
        fn render_chunk(
            &self,
            _: usize,
            code: &str,
            file: &str,
            ty: &str,
        ) -> Result<Option<CodeResult>> {
            Ok(Some(CodeResult {
                code: format!("/* banner {file} {ty} */\n{code}"),
                map: None,
            }))
        }
        fn transform_index_html(&self, _: usize, _: &str, _: &str) -> Result<Option<HtmlResult>> {
            Ok(Some(HtmlResult {
                html: None,
                tags: vec![HtmlTagSpec {
                    tag: "meta".into(),
                    attrs: vec![("name".into(), "from-plugin".into())],
                    ..Default::default()
                }],
            }))
        }
    }

    fn hook_project(index_src: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(&root, "index.ts", index_src);
        (tmp, root.clone(), root.join("dist"))
    }

    fn hook_engine(root: &Path, out: &Path) -> BuildEngine {
        let mut cfg = PledgeConfig::default();
        cfg.root = root.to_path_buf();
        cfg.cache.enabled = false;
        cfg.source_maps = false;
        cfg.entry = vec!["index.ts".to_string()];
        cfg.out_dir = out.to_path_buf();
        BuildEngine::new(Arc::new(cfg))
    }

    fn one_chunk(eng: &BuildEngine) -> Vec<EmitChunk> {
        let mut ids: Vec<ModuleId> = eng.modules().keys().copied().collect();
        ids.sort_unstable();
        vec![EmitChunk {
            id: "entry-0".into(),
            modules: ids,
            is_entry: true,
        }]
    }

    #[test]
    fn plugin_hooks_resolve_load_transform_render_and_html_end_to_end() {
        let (_t, root, out) = hook_project(
            "import { greeting } from 'virtual:greeting';\nimport 'cdn-lib';\nexport const n = __ANSWER__;\nconsole.log(greeting);\n",
        );
        let plugin = TestPlugin::default();
        let mut eng = hook_engine(&root, &out);
        eng.build_with_hooks(Some(&plugin)).unwrap();

        // resolveId + load: the virtual module exists with the plugin's code;
        // the external import was not bundled (and did not fail the build).
        let virt = eng
            .modules()
            .values()
            .find(|m| m.path == Path::new("virtual:greeting"))
            .expect("virtual module missing");
        assert!(String::from_utf8_lossy(&virt.source).contains("hello from plugin"));
        assert_eq!(eng.modules().len(), 2, "cdn-lib must stay external");

        // transform: applied to the source before the built-in transform, so
        // both the stored source and the compiled output carry the change.
        let entry = eng
            .modules()
            .values()
            .find(|m| m.path.ends_with("index.ts"))
            .unwrap();
        assert!(String::from_utf8_lossy(&entry.source).contains("= 42"));
        assert!(eng.cached_for(entry).unwrap().code.contains("42"));

        // renderChunk + transformIndexHtml at emit time.
        eng.emit_with_chunks_hooks(&one_chunk(&eng), Some(&plugin))
            .unwrap();
        let js: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".js"))
            .collect();
        assert_eq!(js.len(), 1);
        let name = js[0].file_name().to_string_lossy().to_string();
        let content = std::fs::read_to_string(js[0].path()).unwrap();
        assert!(
            content.starts_with("/* banner entry-0.js entry */"),
            "{content}"
        );
        // The hash in the file name reflects the FINAL (rendered) content.
        let hash = &blake3::hash(content.as_bytes()).to_hex()[..8];
        assert_eq!(name, format!("entry-0.{hash}.js"));
        let html = std::fs::read_to_string(out.join("index.html")).unwrap();
        assert!(html.contains("<meta name=\"from-plugin\">"), "{html}");
        assert!(
            html.contains(&name),
            "html must reference the hashed chunk: {html}"
        );
    }

    #[test]
    fn package_json_imports_specifiers_resolve_for_the_importing_package() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        write(
            &root,
            "package.json",
            r##"{"name":"app","imports":{
                "#utils/*":"./src/utils/*.ts",
                "#cfg":{"browser":"./src/cfg.browser.ts","default":"./src/cfg.ts"},
                "#dep":"dep-pkg",
                "#gone":"./src/does-not-exist.ts"}}"##,
        );
        write(&root, "src/utils/a.ts", "export const a = 1;\n");
        write(&root, "src/cfg.ts", "export const c = 'default';\n");
        write(&root, "src/cfg.browser.ts", "export const c = 'browser';\n");
        write(&root, "src/main.ts", "import '#utils/a';\nimport '#cfg';\n");
        write(
            &root,
            "node_modules/dep-pkg/package.json",
            r#"{"name":"dep-pkg","main":"./main.js"}"#,
        );
        write(
            &root,
            "node_modules/dep-pkg/main.js",
            "export const d = 1;\n",
        );
        // A nested package WITHOUT `imports`: its scope ends at its own
        // package.json, so it cannot use the parent's map.
        write(&root, "packages/inner/package.json", r#"{"name":"inner"}"#);
        write(&root, "packages/inner/x.ts", "export {};\n");

        let mut cfg = PledgeConfig::default();
        cfg.root = root.clone();
        cfg.cache.enabled = false;
        cfg.entry = vec!["src/main.ts".to_string()];
        cfg.out_dir = root.join("dist");
        cfg.conditions = vec!["browser".to_string()];
        let eng = BuildEngine::new(Arc::new(cfg));
        let importer = root.join("src/main.ts");

        let r = |spec: &str, imp: &Path| eng.resolve(spec, Some(&imp.to_path_buf()));
        assert_eq!(
            r("#utils/a", &importer).unwrap(),
            root.join("src/utils/a.ts")
        );
        // Configured condition ("browser") wins over `default`.
        assert_eq!(
            r("#cfg", &importer).unwrap(),
            root.join("src/cfg.browser.ts")
        );
        // Bare package target.
        assert_eq!(
            r("#dep", &importer).unwrap(),
            root.join("node_modules/dep-pkg/main.js")
        );
        // Mapped-but-missing target and unknown key are clear errors.
        let e = r("#gone", &importer).unwrap_err().to_string();
        assert!(e.contains("does not exist"), "{e}");
        let e = r("#nope", &importer).unwrap_err().to_string();
        assert!(e.contains("not defined"), "{e}");
        // Package scope: the inner package has no `imports`.
        let e = r("#utils/a", &root.join("packages/inner/x.ts"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no \"imports\" field"), "{e}");
    }

    #[test]
    fn plugin_transform_error_aborts_the_build_naming_plugin_and_file() {
        let (_t, root, out) = hook_project("export const x = 'FAIL_ME';\n");
        let plugin = TestPlugin::default();
        let mut eng = hook_engine(&root, &out);
        let err = eng.build_with_hooks(Some(&plugin)).unwrap_err().to_string();
        assert!(err.contains("test-plugin"), "{err}");
        assert!(err.contains("transform"), "{err}");
        assert!(err.contains("index.ts"), "{err}");
    }

    /// A transform-only plugin that counts invocations, with a configurable
    /// cache fingerprint and optional source map.
    struct CountingPlugin {
        calls: std::cell::Cell<usize>,
        fingerprint: Option<&'static str>,
        with_map: bool,
    }

    impl PluginHooks for CountingPlugin {
        fn plugin_count(&self) -> usize {
            1
        }
        fn plugin_name(&self, _: usize) -> String {
            "counting".to_string()
        }
        fn supports(&self, _: usize, hook: BuildHook) -> bool {
            hook == BuildHook::Transform
        }
        fn transform_fingerprint(&self, _: usize) -> Option<String> {
            self.fingerprint.map(String::from)
        }
        fn resolve_id(&self, _: usize, _: &str, _: Option<&str>) -> Result<Option<ResolvedId>> {
            Ok(None)
        }
        fn load(&self, _: usize, _: &str) -> Result<Option<CodeResult>> {
            Ok(None)
        }
        fn transform(&self, _: usize, code: &str, _: &str) -> Result<Option<CodeResult>> {
            self.calls.set(self.calls.get() + 1);
            Ok(code.contains("__ANSWER__").then(|| CodeResult {
                code: code.replace("__ANSWER__", "42"),
                // One segment: plugin output (0,0) -> original-source.ts (0,0).
                map: self.with_map.then(|| {
                    r#"{"version":3,"sources":["original-source.ts"],"names":[],"mappings":"AAAA"}"#
                        .to_string()
                }),
            }))
        }
        fn render_chunk(&self, _: usize, _: &str, _: &str, _: &str) -> Result<Option<CodeResult>> {
            Ok(None)
        }
        fn transform_index_html(&self, _: usize, _: &str, _: &str) -> Result<Option<HtmlResult>> {
            Ok(None)
        }
    }

    fn cached_hook_engine(root: &Path, out: &Path, cache_dir: &Path) -> BuildEngine {
        let mut cfg = PledgeConfig::default();
        cfg.root = root.to_path_buf();
        cfg.cache.enabled = true;
        cfg.cache.dir = cache_dir.to_path_buf();
        cfg.source_maps = false;
        cfg.entry = vec!["index.ts".to_string()];
        cfg.out_dir = out.to_path_buf();
        BuildEngine::new(Arc::new(cfg))
    }

    #[test]
    fn plugin_transform_results_are_cached_and_keyed_by_plugin_fingerprint() {
        let (_t, root, out) = hook_project("export const n = __ANSWER__;\n");
        let cache = root.join(".cache-plugin-transform");
        let run = |fingerprint: Option<&'static str>| -> (usize, String) {
            let plugin = CountingPlugin {
                calls: std::cell::Cell::new(0),
                fingerprint,
                with_map: false,
            };
            let mut eng = cached_hook_engine(&root, &out, &cache);
            eng.build_with_hooks(Some(&plugin)).unwrap();
            let src = eng
                .modules()
                .values()
                .find(|m| m.path.ends_with("index.ts"))
                .map(|m| String::from_utf8_lossy(&m.source).to_string())
                .unwrap();
            (plugin.calls.get(), src)
        };

        // First build runs the plugin; identical fingerprint + input afterwards
        // is served from the cache, with the same (post-plugin) source.
        let (calls, src) = run(Some("v1"));
        assert_eq!(calls, 1);
        assert!(src.contains("= 42"), "{src}");
        let (calls, src) = run(Some("v1"));
        assert_eq!(
            calls, 0,
            "same plugin identity/version/config must hit the cache"
        );
        assert!(src.contains("= 42"), "{src}");

        // A different plugin version/config fingerprint must NOT reuse it.
        let (calls, _) = run(Some("v2"));
        assert_eq!(calls, 1, "changed fingerprint must re-run the plugin");

        // No fingerprint = opaque/impure plugin: never cached.
        let (calls, _) = run(None);
        assert_eq!(calls, 1);
        let (calls, _) = run(None);
        assert_eq!(calls, 1, "unfingerprinted plugins always run");
    }

    #[test]
    fn plugin_source_map_is_chained_into_the_emitted_map() {
        let (_t, root, out) = hook_project("export const n = __ANSWER__;\n");
        let plugin = CountingPlugin {
            calls: std::cell::Cell::new(0),
            fingerprint: None,
            with_map: true,
        };
        let mut cfg = PledgeConfig::default();
        cfg.root = root.clone();
        cfg.cache.enabled = false;
        cfg.source_maps = true;
        cfg.entry = vec!["index.ts".to_string()];
        cfg.out_dir = out.clone();
        let mut eng = BuildEngine::new(Arc::new(cfg));
        eng.build_with_hooks(Some(&plugin)).unwrap();
        eng.emit_with_hooks(Some(&plugin)).unwrap();

        fn find_maps(dir: &Path, out: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    find_maps(&p, out);
                } else if p.extension().is_some_and(|x| x == "map") {
                    out.push(p);
                }
            }
        }
        let mut maps = Vec::new();
        find_maps(&out, &mut maps);
        let entry_map = maps
            .iter()
            .find(|p| p.to_string_lossy().contains("index"))
            .unwrap_or_else(|| panic!("no source map emitted: {maps:?}"));
        let map: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(entry_map).unwrap()).unwrap();
        // The plugin's map was composed in: the final map now points at the
        // plugin's ORIGINAL source, not the intermediate plugin output.
        assert_eq!(
            map["sources"],
            serde_json::json!(["original-source.ts"]),
            "{map}"
        );
        assert!(!map["mappings"].as_str().unwrap().is_empty(), "{map}");
    }

    #[test]
    fn virtual_module_without_a_load_hook_is_a_clear_error() {
        struct ResolveOnly;
        impl PluginHooks for ResolveOnly {
            fn plugin_count(&self) -> usize {
                1
            }
            fn plugin_name(&self, _: usize) -> String {
                "resolve-only".into()
            }
            fn supports(&self, _: usize, h: BuildHook) -> bool {
                h == BuildHook::ResolveId
            }
            fn resolve_id(&self, _: usize, s: &str, _: Option<&str>) -> Result<Option<ResolvedId>> {
                Ok((s == "ghost").then(|| ResolvedId {
                    id: "ghost".into(),
                    external: false,
                }))
            }
            fn load(&self, _: usize, _: &str) -> Result<Option<CodeResult>> {
                Ok(None)
            }
            fn transform(&self, _: usize, _: &str, _: &str) -> Result<Option<CodeResult>> {
                Ok(None)
            }
            fn render_chunk(
                &self,
                _: usize,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<Option<CodeResult>> {
                Ok(None)
            }
            fn transform_index_html(
                &self,
                _: usize,
                _: &str,
                _: &str,
            ) -> Result<Option<HtmlResult>> {
                Ok(None)
            }
        }
        let (_t, root, out) = hook_project("import 'ghost';\n");
        let mut eng = hook_engine(&root, &out);
        let err = eng
            .build_with_hooks(Some(&ResolveOnly))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ghost") && err.contains("load"), "{err}");
    }

    #[test]
    fn build_without_hooks_is_unchanged() {
        let (_t, root, out) = hook_project("export const n = __ANSWER__;\n");
        let mut eng = hook_engine(&root, &out);
        eng.build_with_hooks(None).unwrap();
        let entry = eng.modules().values().next().unwrap();
        assert!(String::from_utf8_lossy(&entry.source).contains("__ANSWER__"));
    }

    // ─── Engine fixes ───────────────────────────────────────────────────

    #[test]
    fn function_cache_does_not_share_entries_between_paths_or_kinds() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (a, b) = (root.join("a.ts"), root.join("dir/a.tsx"));
        let same = "export const x = 1;";
        let mut eng = engine_with_modules(&root, &root.join("dist"), "a.ts", &[]);
        for (path, out) in [(&a, "OUT_TS"), (&b, "OUT_TSX")] {
            let id = eng
                .add_module_with_source(path.clone(), same.as_bytes().to_vec(), None)
                .unwrap();
            let hash = eng.modules[&id].content_hash;
            eng.function_cache
                .insert(module_cache_key(hash, path), cached(out));
        }
        let ma = eng.modules.values().find(|m| m.path == a).unwrap();
        let mb = eng.modules.values().find(|m| m.path == b).unwrap();
        assert_eq!(
            ma.content_hash, mb.content_hash,
            "premise: identical content"
        );
        assert_eq!(eng.cached_for(ma).unwrap().code, "OUT_TS");
        assert_eq!(eng.cached_for(mb).unwrap().code, "OUT_TSX");
    }

    #[test]
    fn worker_files_get_the_worker_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut eng = engine_with_modules(&root, &root.join("dist"), "a.ts", &[]);
        let id = eng
            .add_module_with_source(
                root.join("job.worker.ts"),
                b"self.onmessage=()=>{}".to_vec(),
                None,
            )
            .unwrap();
        assert_eq!(eng.modules[&id].kind, ModuleKind::Worker);
    }

    #[test]
    fn unchanged_prev_modules_is_keyed_by_path_not_shifting_ids() {
        // Previous build: a(0) -> b(1); c(2) unrelated.
        let mut prev = SerializableModuleGraph::new();
        prev.add_module(0, "/p/a.ts".into(), ModuleKind::TypeScript, 10);
        prev.add_module(1, "/p/b.ts".into(), ModuleKind::TypeScript, 20);
        prev.add_module(2, "/p/c.ts".into(), ModuleKind::TypeScript, 30);
        prev.add_dependency(0, 1);
        // This build discovered the modules in another order (ids differ —
        // they are not even part of the input); b changed.
        let current = vec![
            (PathBuf::from("/p/c.ts"), 30),
            (PathBuf::from("/p/b.ts"), 999),
            (PathBuf::from("/p/a.ts"), 10),
        ];
        let reusable = unchanged_prev_modules(&prev, &current);
        // b changed and a depends on it → only c is reusable. With id-based
        // comparison c(now id 0) would have been matched against a(0).
        assert_eq!(reusable, vec![(PathBuf::from("/p/c.ts"), 30)]);
    }

    #[test]
    fn sanitize_chunk_id_cannot_escape_or_produce_bad_names() {
        assert_eq!(sanitize_chunk_id("entry-0"), "entry-0");
        assert_eq!(sanitize_chunk_id("vendor.react"), "vendor.react");
        for bad in ["../../evil", "a/b\\c", "we:ird*name?", "..", "", "."] {
            let s = sanitize_chunk_id(bad);
            assert!(
                !s.contains('/') && !s.contains('\\') && !s.contains(':'),
                "{s}"
            );
            assert!(!s.starts_with('.') && !s.contains(".."), "{s}");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn emit_with_chunks_sanitises_ids_in_file_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let out = root.join("dist");
        let eng = engine_with_modules(&root, &out, "a.ts", &[(root.join("a.ts"), "A")]);
        eng.emit_with_chunks(&[EmitChunk {
            id: "../../escape:me".into(),
            modules: vec![0],
            is_entry: true,
        }])
        .unwrap();
        assert!(!root.join("escape").exists());
        let names: Vec<String> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.contains("escape_me.") && n.ends_with(".js") && !n.contains('/')),
            "{names:?}"
        );
        // The manifest is still keyed by the original id.
        let manifest = std::fs::read_to_string(out.join("manifest.json")).unwrap();
        assert!(manifest.contains("../../escape:me"));
    }

    #[test]
    fn emit_with_chunks_errors_when_a_module_is_missing_from_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let eng = engine_with_modules(
            &root,
            &root.join("dist"),
            "a.ts",
            &[(root.join("a.ts"), "A"), (root.join("b.ts"), "B")],
        );
        let key = {
            let m = &eng.modules[&1];
            module_cache_key(m.content_hash, &m.path)
        };
        eng.function_cache.remove(&key);
        let chunk = |modules: Vec<ModuleId>| EmitChunk {
            id: "entry-0".into(),
            modules,
            is_entry: true,
        };
        let err = eng
            .emit_with_chunks(&[chunk(vec![0, 1])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("b.ts") && err.contains("missing"), "{err}");
        // A module id the build never produced is an error too.
        let err = eng
            .emit_with_chunks(&[chunk(vec![0, 77])])
            .unwrap_err()
            .to_string();
        assert!(err.contains("77"), "{err}");
    }

    #[test]
    fn emit_with_chunks_merges_module_source_maps_into_a_correct_chunk_map() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let out = root.join("dist");
        let eng = engine_with_modules(
            &root,
            &out,
            "a.ts",
            &[(root.join("a.ts"), "x"), (root.join("b.ts"), "y")],
        );
        let map = |src: &str| {
            format!(r#"{{"version":3,"sources":["{src}"],"names":[],"mappings":"AAAA"}}"#)
        };
        for (id, code, src) in [
            (0u32, "l1\nl2\n//# sourceMappingURL=a.js.map", "a.ts"),
            (1u32, "m1\n//# sourceMappingURL=b.js.map", "b.ts"),
        ] {
            let m = eng.modules[&id].clone();
            let mut c = cached(code);
            c.source_map = Some(map(src));
            eng.function_cache
                .insert(module_cache_key(m.content_hash, &m.path), c);
        }
        eng.emit_with_chunks(&[EmitChunk {
            id: "entry-0".into(),
            modules: vec![0, 1],
            is_entry: true,
        }])
        .unwrap();
        let entries: Vec<_> = std::fs::read_dir(&out).unwrap().flatten().collect();
        let js = entries
            .iter()
            .find(|e| e.file_name().to_string_lossy().ends_with(".js"))
            .unwrap();
        let map_file = entries
            .iter()
            .find(|e| e.file_name().to_string_lossy().ends_with(".js.map"))
            .expect("chunk map missing");
        let code = std::fs::read_to_string(js.path()).unwrap();
        // Exactly one sourceMappingURL: the chunk's own, not the modules'.
        assert_eq!(code.matches("sourceMappingURL").count(), 1, "{code}");
        assert!(code.contains(&format!(
            "sourceMappingURL={}.map",
            js.file_name().to_string_lossy()
        )));
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(map_file.path()).unwrap()).unwrap();
        let sections = merged["sections"].as_array().expect("indexed map");
        assert_eq!(sections.len(), 2, "both modules' maps must be present");
        assert_eq!(sections[0]["offset"]["line"], 0);
        // module a is "l1\nl2\n" = 2 lines + the joining newline → b starts at line 2.
        assert_eq!(sections[1]["offset"]["line"], 2);
        assert_eq!(sections[0]["map"]["sources"][0], "a.ts");
        assert_eq!(sections[1]["map"]["sources"][0], "b.ts");
        // The offset really points at module b's first line in the chunk.
        assert_eq!(code.lines().nth(2), Some("m1"));
    }
}
