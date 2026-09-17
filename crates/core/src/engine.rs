// Build engine: orchestrates the entire build pipeline
//
// This is the "Turbo engine" equivalent — a function-level
// incremental computation system that caches aggressively
// and only recomputes what changed.

use crate::config::PledgeConfig;
use crate::module::{ModuleId, ResolvedModule};
use crate::module_graph::SerializableModuleGraph;
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
                        lock_path.display()
                    );
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!("lock retry loop always returns")
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
            remote_cache,
            module_graph: SerializableModuleGraph::new(),
            previous_graph,
            git_invalidator: Some(git_invalidator).filter(|g| g.is_available()),
            is_incremental,
            auto_entries: Vec::new(),
            entry_module_ids: Vec::new(),
            i18n_catalog: crate::i18n::TranslationCatalog::default(),
        }
    }

    /// Run a full build (dev or production)
    ///
    /// Supports incremental rebuilds: if a previous module graph was loaded
    /// from disk, only changed modules and their dependents are re-transformed.
    /// Unchanged modules are loaded from cache (memory → disk → remote).
    pub async fn build(&mut self) -> Result<BuildResult> {
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
            if let Err(e) = self.resolve_and_add(entry, None) {
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

        // Phase 2: Determine which modules need rebuilding (incremental)
        let mut skip_set: HashSet<ModuleId> = HashSet::new();
        if self.is_incremental
            && let Some(ref prev) = self.previous_graph
        {
            // Use git tree hash for fast repo-level change detection
            if let Some(ref git) = self.git_invalidator {
                let prev_tree = prev.git_tree_hash.as_deref();
                if !git.has_repo_changed(prev_tree) {
                    info!("Git tree hash unchanged — full cache hit");
                    // Load all modules from previous graph
                    for (id, node) in &prev.modules {
                        if let Some(cached) =
                            self.load_cached_module(*id, node.content_hash, &node.path)
                        {
                            self.function_cache.insert(node.content_hash, cached);
                            skip_set.insert(*id);
                        }
                    }
                }
            }

            // If git invalidation isn't available or tree changed,
            // use content-hash-based incremental detection
            if skip_set.is_empty() {
                // Compare current entry content hashes with previous
                let mut changed: HashSet<ModuleId> = HashSet::new();
                for (id, module) in &self.modules {
                    if let Some(prev_node) = prev.modules.get(id) {
                        if module.content_hash != prev_node.content_hash {
                            debug!(
                                "Changed module: {:?} (hash {} → {})",
                                module.path, prev_node.content_hash, module.content_hash
                            );
                            changed.insert(*id);
                        }
                    } else {
                        changed.insert(*id);
                    }
                }
                // Compute affected dependents (transitive closure)
                let affected = self.module_graph.compute_affected(&changed);
                // Load unaffected modules from cache
                for (id, node) in &prev.modules {
                    if !affected.contains(id)
                        && let Some(cached) =
                            self.load_cached_module(*id, node.content_hash, &node.path)
                    {
                        self.function_cache.insert(node.content_hash, cached);
                        skip_set.insert(*id);
                    }
                }
                if !affected.is_empty() {
                    info!(
                        "Incremental: {} changed, {} affected, {} cached",
                        changed.len(),
                        affected.len(),
                        skip_set.len()
                    );
                }
            }
        }

        // Phase 3a: BFS Resolution — discover all modules using fast SIMD scanning.
        // Deps come from find_imports + extract_module_specifier, NOT from Oxc transform.
        // Uncached modules are collected for parallel transformation in Phase 3b.
        let mut modules_built = 0usize;
        let mut modules_cached = 0usize;
        let mut pending_transforms: Vec<(ModuleId, ResolvedModule)> = Vec::new();

        // Sort the initial queue by ModuleId for deterministic build ordering.
        // Using FIFO (remove(0)) instead of LIFO (pop) ensures modules are
        // processed in a stable, reproducible order across builds, which is
        // critical for reproducible output hashes and cache keys.
        let mut queue: Vec<ModuleId> = self.path_to_id.values().copied().collect();
        queue.sort_unstable();
        queue.dedup();
        let mut processed = HashSet::new();

        while !queue.is_empty() {
            // Sort to maintain deterministic order after new deps are pushed.
            queue.sort_unstable();
            queue.dedup();
            let module_id = queue.remove(0); // FIFO for deterministic ordering
            if processed.contains(&module_id) {
                continue;
            }
            processed.insert(module_id);

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

            let cache_key = module.content_hash;

            // Skip if already loaded from incremental cache
            if skip_set.contains(&module_id) {
                modules_cached += 1;
                if let Some(cached) = self
                    .function_cache
                    .get(&module.content_hash)
                    .map(|r| r.value().clone())
                {
                    for dep_path in &cached.deps {
                        let dep_id = self.resolve_and_add(dep_path, Some(&module.path))?;
                        queue.push(dep_id);
                    }
                }
                continue;
            }

            // Check function-level cache (memory first, then disk, then remote)
            if let Some(cached) = self
                .function_cache
                .get(&cache_key)
                .map(|r| r.value().clone())
            {
                modules_cached += 1;
                for dep_path in &cached.deps {
                    let dep_id = self.resolve_and_add(dep_path, Some(&module.path))?;
                    queue.push(dep_id);
                }
                continue;
            }

            if let Some(ref pc) = self.persistent_cache {
                let pkey = pledgepack_cache::make_key(
                    cache_key,
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
                        let dep_id = self.resolve_and_add(dep_path, Some(&module.path))?;
                        queue.push(dep_id);
                    }
                    continue;
                }

                if let Some(ref rc) = self.remote_cache {
                    // Try remote cache before transforming
                    let rkey = pledgepack_cache::remote::remote_cache_key(
                        cache_key,
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
                            let dep_id = self.resolve_and_add(dep_path, Some(&module.path))?;
                            queue.push(dep_id);
                        }
                        continue;
                    }
                }
            }

            // Not in any cache — discover deps via SIMD scanning for BFS, defer transform
            let source_str = String::from_utf8_lossy(&module.source).to_string();
            let import_offsets = pledgepack_native_sys::find_imports(&module.source);
            for offset in import_offsets {
                let rest = &source_str[offset..];
                if let Some(dep) = extract_module_specifier(rest) {
                    let dep_id = self.resolve_and_add(&dep, Some(&module.path))?;
                    queue.push(dep_id);
                }
            }
            pending_transforms.push((module_id, module));
        }

        // Phase 3b: Transform all uncached modules in parallel using rayon
        if !pending_transforms.is_empty() {
            modules_built += pending_transforms.len();
            let parallel_results = self.transform_modules_parallel(pending_transforms)?;

            // Phase 3c: Populate caches from parallel results
            for (module_id, output) in parallel_results {
                let Some(module) = self.modules.get(&module_id) else {
                    warn!(
                        "Module {:?} vanished between transform and cache write",
                        module_id
                    );
                    continue;
                };
                let cache_key = module.content_hash;

                if let Some(ref pc) = self.persistent_cache {
                    let pkey = pledgepack_cache::make_key(
                        cache_key,
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

                if let Some(ref rc) = self.remote_cache {
                    let rkey = pledgepack_cache::remote::remote_cache_key(
                        cache_key,
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
            if let Some(cached) = self.function_cache.get(&module.content_hash) {
                for dep_path in &cached.deps {
                    if let Ok(dep_path_resolved) = self.resolve(dep_path, Some(&module.path))
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

    /// Try to load a cached module output from persistent cache
    fn load_cached_module(
        &self,
        _id: ModuleId,
        content_hash: u64,
        path: &Path,
    ) -> Option<CachedOutput> {
        if let Some(cached) = self.function_cache.get(&content_hash) {
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

    /// Resolve a module path and add it to the graph.
    /// `importer` is the path of the importing module (for relative resolution).
    fn resolve_and_add(&mut self, specifier: &str, importer: Option<&PathBuf>) -> Result<ModuleId> {
        let path = self.resolve(specifier, importer)?;

        if let Some(&id) = self.path_to_id.get(&path) {
            return Ok(id);
        }

        let id = self
            .graph
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add_module(path.to_str().unwrap_or(""));
        self.path_to_id.insert(path.clone(), id);

        // Read source via Zig I/O layer
        let source = pledgepack_native_sys::read_file(path.to_str().unwrap_or(""))?;
        let content_hash =
            u64::from_be_bytes(blake3::hash(&source).as_bytes()[0..8].try_into().unwrap());

        let ext_str = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e))
            .unwrap_or_default();
        let kind = crate::module::ModuleKind::from_extension(&ext_str);

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
            || (module_count > WARN_MODULES && module_count % WARN_MODULES == 0)
        {
            warn!(
                "Large module count ({}). Consider increasing memory limits or splitting the build.",
                module_count
            );
        }

        Ok(id)
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

        // 0. Check path aliases (e.g., "@/components" → "src/components")
        for alias in &self.config.alias {
            if specifier.starts_with(&alias.from) {
                let rest = &specifier[alias.from.len()..];
                // Ensure we match at a path boundary: alias "@/components" should not match "@/components-extra"
                if !rest.is_empty() && !rest.starts_with('/') {
                    continue;
                }
                let alias_path = PathBuf::from(&alias.to);
                let path = if rest.is_empty() {
                    alias_path
                } else {
                    alias_path.join(rest.trim_start_matches('/'))
                };
                // Try with extensions
                for ext in &self.config.extensions {
                    let with_ext = if path.extension().is_some() {
                        path.clone()
                    } else {
                        path.with_extension(ext.trim_start_matches('.'))
                    };
                    if with_ext.exists() {
                        return Ok(with_ext);
                    }
                }
                // Try as directory with index file
                if path.is_dir() {
                    for ext in &self.config.extensions {
                        let index = path.join(format!("index{}", ext));
                        if index.exists() {
                            return Ok(index);
                        }
                    }
                }
            }
        }

        // Handle relative paths
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let base = importer
                .and_then(|p| p.parent())
                .unwrap_or(&self.config.root);
            let path = base.join(specifier);

            // Try with extensions
            for ext in &self.config.extensions {
                let with_ext = if path.extension().is_some() {
                    path.clone()
                } else {
                    path.with_extension(ext.trim_start_matches('.'))
                };
                if with_ext.exists() {
                    return Ok(with_ext);
                }
            }

            // Try as directory with index file
            for ext in &self.config.extensions {
                let index = path.join(format!("index{}", ext));
                if index.exists() {
                    return Ok(index);
                }
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

                let pkg_json = node_modules.join(&pkg_name).join("package.json");

                if pkg_json.exists() {
                    let content = read_file_mmap(&pkg_json)?;
                    let pkg: serde_json::Value = serde_json::from_str(&content)?;

                    if subpath.is_empty() {
                        // Root import: check "exports" field first (modern), then "module"/"main"
                        if let Some(exports) = pkg.get("exports") {
                            // Sugar form: { "import": "...", "require": "..." } or { ".": { "import": "..." } }
                            if let Some(obj) = exports.as_object() {
                                // Check if it's sugar form (top-level conditions)
                                if obj.contains_key("import")
                                    || obj.contains_key("require")
                                    || obj.contains_key("default")
                                    || obj.contains_key("browser")
                                {
                                    let resolved = obj
                                        .get("browser")
                                        .or_else(|| obj.get("module"))
                                        .or_else(|| obj.get("import"))
                                        .or_else(|| obj.get("require"))
                                        .or_else(|| obj.get("default"))
                                        .and_then(|v| v.as_str());
                                    if let Some(entry) = resolved {
                                        let full = node_modules
                                            .join(&pkg_name)
                                            .join(entry.trim_start_matches("./"));
                                        if full.exists() {
                                            return Ok(full);
                                        }
                                    }
                                } else if let Some(dot_export) = obj.get(".") {
                                    // "." key with conditions
                                    let resolved = if let Some(s) = dot_export.as_str() {
                                        Some(s.to_string())
                                    } else if let Some(conditions) = dot_export.as_object() {
                                        conditions
                                            .get("browser")
                                            .or_else(|| conditions.get("module"))
                                            .or_else(|| conditions.get("import"))
                                            .or_else(|| conditions.get("default"))
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string())
                                    } else {
                                        None
                                    };
                                    if let Some(entry) = resolved {
                                        let full = node_modules
                                            .join(&pkg_name)
                                            .join(entry.trim_start_matches("./"));
                                        if full.exists() {
                                            return Ok(full);
                                        }
                                    }
                                }
                            } else if let Some(entry) = exports.as_str() {
                                // Direct string export
                                let full = node_modules
                                    .join(&pkg_name)
                                    .join(entry.trim_start_matches("./"));
                                if full.exists() {
                                    return Ok(full);
                                }
                            }
                        }

                        // Fallback: resolve via "module" or "main"
                        let entry = pkg
                            .get("module")
                            .or_else(|| pkg.get("main"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("index.js");
                        let full = node_modules.join(&pkg_name).join(entry);
                        if full.exists() || full == node_modules.join(&pkg_name).join("index.js") {
                            return Ok(full);
                        }
                        // Try index.js in the package directory
                        let index = node_modules.join(&pkg_name).join("index.js");
                        if index.exists() {
                            return Ok(index);
                        }
                        return Ok(full); // Return even if doesn't exist — error will surface on read
                    } else {
                        // Subpath import: check "exports" map first
                        if let Some(exports) = pkg.get("exports")
                            && let Some(obj) = exports.as_object()
                        {
                            let key = format!(".{}", subpath);
                            if let Some(export_val) = obj.get(&key) {
                                let resolved = if let Some(s) = export_val.as_str() {
                                    Some(s.to_string())
                                } else if let Some(conditions) = export_val.as_object() {
                                    conditions
                                        .get("browser")
                                        .or_else(|| conditions.get("module"))
                                        .or_else(|| conditions.get("import"))
                                        .or_else(|| conditions.get("default"))
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                } else {
                                    None
                                };
                                if let Some(resolved_path) = resolved {
                                    let full = node_modules
                                        .join(&pkg_name)
                                        .join(resolved_path.trim_start_matches("./"));
                                    if full.exists() {
                                        return Ok(full);
                                    }
                                }
                            }
                        }
                        // Fallback: try direct file path
                        let direct = node_modules
                            .join(&pkg_name)
                            .join(subpath.trim_start_matches('/'));
                        if direct.exists() {
                            return Ok(direct);
                        }
                        // Try with extensions
                        for ext in &self.config.extensions {
                            let with_ext = direct.with_extension(ext.trim_start_matches('.'));
                            if with_ext.exists() {
                                return Ok(with_ext);
                            }
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
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(parallelism)
            .build()
            .unwrap_or_else(|_| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(4)
                    .build()
                    .unwrap_or_else(|_| {
                        // Last resort: single-threaded pool — always works
                        rayon::ThreadPoolBuilder::new()
                            .num_threads(1)
                            .build()
                            .expect("single-threaded rayon pool must succeed")
                    })
            });

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
                    let import_offsets = pledgepack_native_sys::find_imports(&module.source);

                    let mut deps = Vec::new();
                    for offset in import_offsets {
                        let rest = &source_str[offset..];
                        if let Some(dep) = extract_module_specifier(rest) {
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

    /// Get a locked reference to the module graph (for dev server / HMR).
    /// Returns a `MutexGuard` — the lock is held for the guard's lifetime.
    pub fn graph(&self) -> std::sync::MutexGuard<'_, Graph> {
        self.graph.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get all resolved modules
    pub fn modules(&self) -> &HashMap<ModuleId, ResolvedModule> {
        &self.modules
    }

    /// Get the entry module IDs (modules resolved from config entry points).
    /// Returns only the actual entry points, not all resolved modules.
    pub fn entry_ids(&self) -> Vec<ModuleId> {
        if !self.entry_module_ids.is_empty() {
            self.entry_module_ids.clone()
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
        if chunks.is_empty() {
            return self.emit();
        }

        let out_dir = &self.config.out_dir;
        // Safety check: never delete the project root or source directories
        if out_dir == &self.config.root {
            anyhow::bail!("Output directory cannot be the same as project root");
        }
        // Acquire the output lock BEFORE wiping out_dir — two concurrent
        // builds writing to the same directory would corrupt each other.
        let _output_lock = OutputLock::acquire(out_dir)?;
        if out_dir.exists() {
            let canonical = out_dir.canonicalize().unwrap_or(out_dir.to_path_buf());
            if canonical.parent().is_none() {
                anyhow::bail!(
                    "Refusing to delete unsafe output directory: {}",
                    canonical.display()
                );
            }
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;

        let mut css_files: Vec<String> = Vec::new();
        let mut js_files: Vec<String> = Vec::new();
        let mut async_chunks: Vec<String> = Vec::new();
        let mut manifest_entries: std::collections::HashMap<String, ManifestEntry> =
            std::collections::HashMap::new();
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

            for &module_id in &chunk.modules {
                let module = match self.modules.get(&module_id) {
                    Some(m) => m,
                    None => continue,
                };
                if let Some(cached) = self.function_cache.get(&module.content_hash) {
                    if cached.is_css {
                        chunk_css.push_str(&cached.code);
                        chunk_css.push('\n');
                        has_css = true;
                    } else {
                        chunk_content.push_str(&cached.code);
                        chunk_content.push('\n');
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
            }

            // Compute content hash for the chunk filename
            let hash = blake3::hash(chunk_content.as_bytes());
            let hash_hex = &hash.to_hex()[..8];
            let chunk_filename = format!("{}.{}.js", chunk.id, hash_hex);
            let chunk_path = out_dir.join(&chunk_filename);
            let chunk_rel = crate::normalize_path_str(&chunk_filename);

            // Create parent directories
            if let Some(parent) = chunk_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Write the chunk file
            write_output_file(&chunk_path, &chunk_content)?;
            tracing::info!(
                "Emitted chunk: {} ({} modules)",
                chunk_path.display(),
                chunk.modules.len()
            );

            // Write source map if present (use first module's source map)
            if let Some(first_module_id) = chunk.modules.first()
                && let Some(module) = self.modules.get(first_module_id)
                && let Some(cached) = self.function_cache.get(&module.content_hash)
                && let Some(ref source_map) = cached.source_map
            {
                let mode = &self.config.build.source_map_mode;
                if mode != "hidden" {
                    let map_path = chunk_path.with_extension("js.map");
                    std::fs::write(&map_path, source_map)?;
                }
            }

            // Write extracted CSS as a separate file if any
            if has_css && !chunk_css.is_empty() {
                let css_hash = blake3::hash(chunk_css.as_bytes());
                let css_hash_hex = &css_hash.to_hex()[..8];
                let css_filename = format!("{}.{}.css", chunk.id, css_hash_hex);
                let css_path = out_dir.join(&css_filename);
                let css_rel = crate::normalize_path_str(&css_filename);
                std::fs::write(&css_path, &chunk_css)?;
                css_files.push(css_rel.clone());
                tracing::info!("Emitted chunk CSS: {}", css_path.display());
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
                catalog_path.display()
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

        let html_path = out_dir.join("index.html");
        std::fs::write(&html_path, html)?;
        info!("Generated: {}", html_path.display());

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
        let out_dir = &self.config.out_dir;
        // Safety check: never delete the project root or source directories
        if out_dir == &self.config.root {
            anyhow::bail!("Output directory cannot be the same as project root");
        }
        // Acquire the output lock BEFORE wiping out_dir (see emit_with_chunks).
        let _output_lock = OutputLock::acquire(out_dir)?;
        // Safety: refuse to delete root or filesystem root
        if out_dir.exists() {
            let canonical = out_dir.canonicalize().unwrap_or(out_dir.to_path_buf());
            if canonical.parent().is_none() {
                anyhow::bail!(
                    "Refusing to delete unsafe output directory: {}",
                    canonical.display()
                );
            }
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;

        let mut css_files: Vec<String> = Vec::new();
        let mut js_files: Vec<String> = Vec::new();
        let mut async_chunks: Vec<String> = Vec::new();
        let mut manifest_entries: std::collections::HashMap<String, ManifestEntry> =
            std::collections::HashMap::new();
        let mut entry_chunks: Vec<(String, String)> = Vec::new(); // (entry name, hashed filename)

        // Determine entry modules from config or auto-discovered entries
        let entries: Vec<String> = if !self.auto_entries.is_empty() {
            self.auto_entries.clone()
        } else {
            self.config.entry.clone()
        };

        // Write each transformed module to .pledge/
        for module in self.modules.values() {
            if let Some(cached) = self.function_cache.get(&module.content_hash) {
                // Determine output path relative to project root
                let rel = module
                    .path
                    .strip_prefix(&self.config.root)
                    .unwrap_or(&module.path);
                let out_path = out_dir.join(rel);

                // Compute content hash for filename
                let hash = blake3::hash(cached.code.as_bytes());
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
                let original_rel = crate::normalize_path(rel);
                let is_entry = entries.iter().any(|e| {
                    let entry_normalized = crate::normalize_path_str(e);
                    original_rel == *e
                        || original_rel == entry_normalized
                        || original_rel.ends_with(&entry_normalized)
                });
                let is_async = !cached.is_css
                    && !cached.dynamic_imports.is_empty()
                    && !self.config.build.inline_dynamic_imports
                    && !is_entry;

                // Incremental output (#54): skip writing if file exists with identical content
                if self.config.build.incremental_output
                    && out_path.exists()
                    && let Ok(existing) = std::fs::read_to_string(&out_path)
                    && existing == cached.code
                {
                    tracing::debug!("Skipped unchanged: {}", out_path.display());
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
                write_output_file(&out_path, &cached.code)?;
                tracing::info!("Emitted: {}", out_path.display());

                // Write source map if present (respecting source_map_mode)
                if let Some(ref source_map) = cached.source_map {
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
                            tracing::info!("RTL CSS: {}", rtl_path.display());
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
                        tracing::info!("Extracted CSS: {}", css_out_path.display());

                        // RTL CSS auto-generation (#107)
                        if crate::rtl::should_generate_rtl(&self.config.css)
                            && let Some(rtl_css) =
                                crate::rtl::generate_rtl_css(extracted_css, &self.config.css)
                        {
                            let rtl_name = format!("{}.{}.rtl.css", css_stem, css_hash_hex);
                            let rtl_out_path = out_path.with_file_name(rtl_name);
                            std::fs::write(&rtl_out_path, &rtl_css)?;
                            tracing::info!("RTL CSS: {}", rtl_out_path.display());
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
                for module in self.modules.values() {
                    if let Some(cached) = self.function_cache.get(&module.content_hash) {
                        let path_str = crate::normalize_path(&module.path);
                        if glob_set.is_match(&path_str)
                            || module_patterns
                                .iter()
                                .any(|pattern| path_str.contains(pattern) || path_str == *pattern)
                        {
                            let rel = module
                                .path
                                .strip_prefix(&self.config.root)
                                .unwrap_or(&module.path);
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
                catalog_path.display()
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
            for module in self.modules.values() {
                if crate::svg::is_svg(&module.path)
                    && let Some(cached) = self.function_cache.get(&module.content_hash)
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
        manifest_entries: &std::collections::HashMap<String, ManifestEntry>,
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
        let out_dir = &self.config.out_dir;
        if out_dir == &self.config.root {
            anyhow::bail!("Output directory cannot be the same as project root");
        }
        // Acquire the output lock BEFORE wiping out_dir (see emit_with_chunks).
        let _output_lock = OutputLock::acquire(out_dir)?;
        if out_dir.exists() {
            // Safety: refuse to delete root, home, or empty paths
            let canonical = out_dir.canonicalize().unwrap_or(out_dir.to_path_buf());
            if canonical == std::path::Path::new("/") || canonical.parent().is_none() {
                bail!(
                    "Refusing to delete unsafe output directory: {}",
                    canonical.display()
                );
            }
            std::fs::remove_dir_all(out_dir)?;
        }
        std::fs::create_dir_all(out_dir)?;

        let mut bundle = String::new();
        let mut css_bundle = String::new();

        // Collect all module codes in dependency order
        let mut visited = std::collections::HashSet::new();
        let entry_id = self.modules.values().next().map(|m| m.id);

        if let Some(entry) = entry_id {
            self.collect_module_code(entry, &mut visited, &mut bundle, &mut css_bundle);
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
        std::fs::write(out_dir.join("index.html"), html)?;

        // Write i18n translation catalog (#13) if i18n extraction is enabled and keys were extracted
        if self.config.i18n.enabled && self.config.i18n.extract && !self.i18n_catalog.is_empty() {
            let catalog_path = out_dir.join("i18n-catalog.json");
            std::fs::write(&catalog_path, self.i18n_catalog.to_json())?;
            info!(
                "i18n: extracted {} translation keys → {}",
                self.i18n_catalog.len(),
                catalog_path.display()
            );
        }

        Ok(())
    }

    /// Recursively collect module code in dependency order for single-file bundle
    pub fn collect_module_code(
        &self,
        module_id: ModuleId,
        visited: &mut std::collections::HashSet<ModuleId>,
        bundle: &mut String,
        css_bundle: &mut String,
    ) {
        if !visited.insert(module_id) {
            return;
        }

        if let Some(module) = self.modules.get(&module_id) {
            // Add this module's code
            if let Some(cached) = self.function_cache.get(&module.content_hash) {
                if cached.is_css {
                    css_bundle.push_str(&cached.code);
                    css_bundle.push('\n');
                } else {
                    let rel = module
                        .path
                        .strip_prefix(&self.config.root)
                        .unwrap_or(&module.path);
                    bundle.push_str(&format!("// === {} ===\n", rel.display()));
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
                self.function_cache.remove(&module.content_hash);
            }

            // Also invalidate dependents
            for dep_id in &dependents {
                if let Some(module) = self.modules.get(dep_id) {
                    self.function_cache.remove(&module.content_hash);
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
fn extract_module_specifier(source: &str) -> Option<String> {
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
mod tests {
    use super::*;

    #[test]
    fn test_extract_module_specifier() {
        assert_eq!(
            extract_module_specifier("import React from 'react'"),
            Some("react".to_string())
        );
        assert_eq!(
            extract_module_specifier("import { foo } from \"./bar\""),
            Some("./bar".to_string())
        );
        assert_eq!(
            extract_module_specifier("import('./lazy')"),
            Some("./lazy".to_string())
        );
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
}
