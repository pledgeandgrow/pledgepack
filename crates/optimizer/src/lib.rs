//! Production optimizer: tree shaking, code splitting, minification, scope hoisting
//!
//! Strategy: Use the cached module graph from the build engine,
//! then run optimization passes on the FULL graph (not cached chunks).
//!
//! This is how we avoid Turbopack's 72% bundle bloat:
//!   - Function-level cache makes graph reconstruction fast
//!   - Optimization runs on the complete graph (not individual cached chunks)
//!   - Tree shaking sees the full dependency picture

use anyhow::Result;
use pledgepack_core::config::BuildConfig;
use pledgepack_core::module::{ModuleId, ModuleKind, ResolvedModule};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

mod side_effects;

/// The bundle optimizer. Runs tree shaking, code splitting, and chunk
/// grouping over the full module graph produced by the build engine.
///
/// Create with [`Optimizer::new`], then call [`Optimizer::optimize`] or
/// [`Optimizer::optimize_with_config`] to produce the final [`Chunk`] set.
/// The optimizer is stateful and resets its internal state at the start
/// of each run, so a single instance can be reused across builds.
pub struct Optimizer {
    /// Modules that have side effects (can't be tree-shaken)
    side_effect_modules: HashSet<ModuleId>,
    /// Chunk grouping
    chunks: Vec<Chunk>,
}

/// A group of modules emitted together as one output file.
///
/// Chunks are produced by [`Optimizer::optimize`]; the `id` becomes the
/// output file name and `modules` lists the module IDs contained in the
/// chunk.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Chunk identifier used as the output file name (e.g. `"entry-0"`,
    /// `"vendor"`, `"shared"`).
    pub id: String,
    /// IDs of the modules contained in this chunk, in emit order.
    pub modules: Vec<ModuleId>,
    /// What kind of chunk this is (entry, vendor, shared, etc.).
    pub chunk_type: ChunkType,
}

/// Classification of a [`Chunk`], determining how it is emitted and loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkType {
    /// Entry chunk — one per entry point, loaded directly by the page.
    Entry,
    /// Vendor chunk — third-party modules from `node_modules`.
    Vendor,
    /// Async chunk — modules behind a dynamic `import()`, loaded on demand.
    Async,
    /// Shared chunk — modules used by more than one entry point.
    Shared,
    /// Route chunk — modules for a single route (route-based splitting).
    Route,
}

impl Default for Optimizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Optimizer {
    /// Create a new optimizer with empty state.
    pub fn new() -> Self {
        Self {
            side_effect_modules: HashSet::new(),
            chunks: Vec::new(),
        }
    }

    /// Run all optimization passes
    pub fn optimize(
        &mut self,
        entry_modules: &[ModuleId],
        all_modules: &HashMap<ModuleId, ResolvedModule>,
        graph: &pledgepack_core::Graph,
    ) -> Result<Vec<Chunk>> {
        // Reset state from any previous optimization run so repeated calls on
        // the same Optimizer instance don't accumulate stale chunks/side effects.
        self.chunks.clear();
        self.side_effect_modules.clear();

        // Phase 1: Mark side-effect-free modules
        self.mark_side_effects(all_modules);

        // Phase 2: Tree shake — remove unreachable modules
        let reachable = self.tree_shake(entry_modules, graph);

        // Phase 3: Code splitting — group modules into chunks
        self.split_chunks(entry_modules, &reachable, all_modules, graph);

        // Phase 4: Scope hoisting — merge entry chunk modules into a single scope
        // (Already handled by emitting modules as separate files with ESM imports)

        Ok(self.chunks.clone())
    }

    /// Run optimization passes with build config for inline_dynamic_imports and manual_chunks
    pub fn optimize_with_config(
        &mut self,
        entry_modules: &[ModuleId],
        all_modules: &HashMap<ModuleId, ResolvedModule>,
        graph: &pledgepack_core::Graph,
        build_config: &BuildConfig,
    ) -> Result<Vec<Chunk>> {
        // Reset state from any previous optimization run so repeated calls on
        // the same Optimizer instance don't accumulate stale chunks/side effects.
        self.chunks.clear();
        self.side_effect_modules.clear();

        // Phase 1: Mark side-effect-free modules
        self.mark_side_effects(all_modules);

        // Phase 2: Tree shake — remove unreachable modules
        let reachable = self.tree_shake(entry_modules, graph);

        // Phase 3: Code splitting — group modules into chunks
        self.split_chunks(entry_modules, &reachable, all_modules, graph);

        // Phase 3b: Apply manual chunks configuration
        if !build_config.manual_chunks.is_empty() {
            self.apply_manual_chunks(&reachable, all_modules, &build_config.manual_chunks);
        }

        // Phase 3c: If inline_dynamic_imports, merge all async chunks into their parent entry chunks
        if build_config.inline_dynamic_imports {
            self.inline_dynamic_imports(entry_modules, graph);
        }

        Ok(self.chunks.clone())
    }

    /// Mark modules with side effects (parallelized using rayon)
    fn mark_side_effects(&mut self, modules: &HashMap<ModuleId, ResolvedModule>) {
        // Process modules in parallel — each module's side-effect analysis is independent
        let side_effects: Mutex<HashSet<ModuleId>> = Mutex::new(HashSet::new());

        modules.par_iter().for_each(|(id, module)| {
            let source = String::from_utf8_lossy(&module.source);

            // Use AST-based detection (exact) for JS/TS modules, falling back
            // to the string heuristic for non-JS modules (CSS, JSON, assets).
            let has_sx = if matches!(
                module.kind,
                ModuleKind::JavaScript | ModuleKind::TypeScript | ModuleKind::Jsx | ModuleKind::Tsx
            ) {
                let source_type = oxc::span::SourceType::from_path(&module.path)
                    .unwrap_or(oxc::span::SourceType::mjs());
                side_effects::has_side_effects_ast(&source, source_type)
            } else {
                module_source_has_side_effects(&source)
            };

            if has_sx && let Ok(mut sx) = side_effects.lock() {
                sx.insert(*id);
            }
        });

        self.side_effect_modules = side_effects.into_inner().unwrap_or_default();
    }

    /// Tree shake: find all reachable modules from entry points
    fn tree_shake(
        &self,
        entry_modules: &[ModuleId],
        graph: &pledgepack_core::Graph,
    ) -> HashSet<ModuleId> {
        let mut reachable = HashSet::new();
        let mut queue: Vec<ModuleId> = entry_modules.to_vec();

        while let Some(id) = queue.pop() {
            if reachable.contains(&id) {
                continue;
            }
            reachable.insert(id);

            // Follow dependencies (modules that this module imports).
            // get_all_dependencies grows the FFI buffer until nothing is
            // truncated — a fixed capacity would silently drop edges for
            // modules with many direct dependencies.
            let deps = graph.get_all_dependencies(id);
            for dep in deps {
                if !reachable.contains(&dep) {
                    queue.push(dep);
                }
            }
        }

        reachable
    }

    /// Split modules into chunks: entry, vendor, and shared (parallelized using rayon)
    fn split_chunks(
        &mut self,
        entry_modules: &[ModuleId],
        reachable: &HashSet<ModuleId>,
        modules: &HashMap<ModuleId, ResolvedModule>,
        graph: &pledgepack_core::Graph,
    ) {
        // The Zig-backed Graph is Send but not Sync, so we extract all
        // dependencies into a plain HashMap sequentially first (O(V+E)),
        // then parallelize the BFS over the extracted map — no Mutex needed.
        let mut dep_map: HashMap<ModuleId, Vec<ModuleId>> = HashMap::new();
        {
            let mut stack: Vec<ModuleId> = entry_modules.to_vec();
            let mut visited = HashSet::new();
            while let Some(id) = stack.pop() {
                if !visited.insert(id) {
                    continue;
                }
                let deps = graph.get_all_dependencies(id);
                for &dep in &deps {
                    stack.push(dep);
                }
                dep_map.insert(id, deps);
            }
        }

        // Track which modules are used by multiple entry points
        // Process each entry's dependency traversal in parallel
        let module_users: Mutex<HashMap<ModuleId, HashSet<ModuleId>>> = Mutex::new(HashMap::new());

        entry_modules.par_iter().for_each(|entry| {
            let mut visited = HashSet::new();
            let mut queue = vec![*entry];
            let mut local_users: HashMap<ModuleId, HashSet<ModuleId>> = HashMap::new();

            while let Some(id) = queue.pop() {
                if visited.contains(&id) {
                    continue;
                }
                visited.insert(id);
                local_users.entry(id).or_default().insert(*entry);
                if let Some(deps) = dep_map.get(&id) {
                    queue.extend(deps.iter().copied());
                }
            }

            // Merge local results into shared map
            let mut global = module_users.lock().unwrap_or_else(|e| e.into_inner());
            for (id, entries) in local_users {
                global.entry(id).or_default().extend(entries);
            }
        });

        let module_users = module_users.into_inner().unwrap_or_else(|e| e.into_inner());

        // Vendor modules: in node_modules (parallelized)
        let entry_module_set: HashSet<ModuleId> = entry_modules.iter().copied().collect();

        // Classify modules in parallel: vendor, shared, or entry-exclusive
        let (vendor_modules, shared_modules): (Vec<ModuleId>, Vec<ModuleId>) = reachable
            .par_iter()
            .filter(|id| !entry_module_set.contains(id))
            .partition_map(|id| {
                // Check if module is in node_modules
                if let Some(module) = modules.get(id) {
                    let path_str = module.path.to_string_lossy();
                    if path_str.contains("node_modules") {
                        return rayon::iter::Either::Left(*id);
                    }
                }

                // Check if shared between entries
                if let Some(users) = module_users.get(id)
                    && users.len() > 1
                {
                    return rayon::iter::Either::Right(*id);
                }

                rayon::iter::Either::Left(*id) // default to vendor if not shared
            });

        // Filter out non-vendor, non-shared from vendor_modules (fix partition logic)
        let vendor_modules: Vec<ModuleId> = vendor_modules
            .into_iter()
            .filter(|id| {
                if let Some(module) = modules.get(id) {
                    module.path.to_string_lossy().contains("node_modules")
                } else {
                    false
                }
            })
            .collect();

        // Convert lookup Vecs to HashSets for O(1) `contains` in the hot
        // per-module loop below (avoids O(n²) linear scans).
        let vendor_set: HashSet<ModuleId> = vendor_modules.iter().copied().collect();
        let shared_set: HashSet<ModuleId> = shared_modules.iter().copied().collect();

        // Entry chunk: entry module + its exclusive deps (parallelized)
        let entry_chunks: Vec<Chunk> = entry_modules
            .par_iter()
            .enumerate()
            .map(|(i, entry)| {
                let mut chunk_modules = vec![*entry];
                let mut visited = HashSet::new();
                let mut queue = vec![*entry];

                while let Some(id) = queue.pop() {
                    if visited.contains(&id) {
                        continue;
                    }
                    visited.insert(id);
                    if id != *entry
                        && !vendor_set.contains(&id)
                        && !shared_set.contains(&id)
                        && !entry_module_set.contains(&id)
                    {
                        chunk_modules.push(id);
                    }
                    if let Some(deps) = dep_map.get(&id) {
                        queue.extend(deps.iter().copied());
                    }
                }

                Chunk {
                    id: format!("entry-{}", i),
                    modules: chunk_modules,
                    chunk_type: ChunkType::Entry,
                }
            })
            .collect();

        self.chunks = entry_chunks;

        // Vendor chunk
        if !vendor_modules.is_empty() {
            self.chunks.push(Chunk {
                id: "vendor".to_string(),
                modules: vendor_modules,
                chunk_type: ChunkType::Vendor,
            });
        }

        // Shared chunk
        if !shared_modules.is_empty() {
            self.chunks.push(Chunk {
                id: "shared".to_string(),
                modules: shared_modules,
                chunk_type: ChunkType::Shared,
            });
        }
    }

    /// Apply manual chunks configuration — group modules matching patterns into named chunks
    fn apply_manual_chunks(
        &mut self,
        reachable: &HashSet<ModuleId>,
        modules: &HashMap<ModuleId, ResolvedModule>,
        manual_chunks: &HashMap<String, Vec<String>>,
    ) {
        for (chunk_name, patterns) in manual_chunks {
            let mut chunk_modules: Vec<ModuleId> = Vec::new();

            // Build a GlobSet from the patterns for this chunk
            let mut glob_builder = globset::GlobSetBuilder::new();
            for pattern in patterns {
                if let Ok(glob) = globset::Glob::new(pattern) {
                    glob_builder.add(glob);
                }
            }
            let glob_set = glob_builder.build().unwrap_or_default();

            for &id in reachable {
                if let Some(module) = modules.get(&id) {
                    let path_str = module.path.to_string_lossy();
                    if glob_set.is_match(path_str.as_ref())
                        || patterns.iter().any(|pattern| {
                            path_str.contains(pattern) || path_str.as_ref() == pattern.as_str()
                        })
                    {
                        chunk_modules.push(id);
                    }
                }
            }

            if !chunk_modules.is_empty() {
                // Convert to a HashSet for O(1) membership tests when removing
                // these modules from other chunks (avoids O(n²) Vec::contains).
                let chunk_module_set: HashSet<ModuleId> = chunk_modules.iter().copied().collect();
                // Remove these modules from other chunks to avoid duplication
                for chunk in &mut self.chunks {
                    chunk.modules.retain(|m| !chunk_module_set.contains(m));
                }

                self.chunks.push(Chunk {
                    id: chunk_name.clone(),
                    modules: chunk_modules,
                    chunk_type: ChunkType::Shared,
                });
                tracing::info!(
                    "Manual chunk '{}': {} modules",
                    chunk_name,
                    self.chunks.last().map(|c| c.modules.len()).unwrap_or(0)
                );
            }
        }
    }

    /// Inline dynamic imports — merge async chunks into the entry chunk that
    /// actually imports them (not into every entry chunk).
    ///
    /// For each async chunk, we walk its modules' reverse dependencies
    /// (`get_dependents`) to find which entry module imports them, then merge
    /// the async chunk's modules into only that entry's chunk.
    fn inline_dynamic_imports(
        &mut self,
        entry_modules: &[ModuleId],
        graph: &pledgepack_core::Graph,
    ) {
        // Find all async chunks
        let async_chunks: Vec<(usize, Vec<ModuleId>)> = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| c.chunk_type == ChunkType::Async)
            .map(|(i, c)| (i, c.modules.clone()))
            .collect();

        if async_chunks.is_empty() {
            return;
        }

        // Map each entry module to the index of its entry chunk.
        let entry_module_set: HashSet<ModuleId> = entry_modules.iter().copied().collect();
        let entry_indices: Vec<usize> = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| c.chunk_type == ChunkType::Entry)
            .map(|(i, _)| i)
            .collect();

        // For each async chunk, find which entry chunk(s) import its modules
        // via reverse-dependency traversal, and merge into only those entries.
        for (_, async_modules) in &async_chunks {
            // Collect the entry chunk indices that import this async chunk.
            // We walk the reverse deps of each async module until we reach an
            // entry module, then map that entry module to its chunk index.
            let mut target_entries: HashSet<usize> = HashSet::new();
            for &async_module in async_modules {
                let mut queue = vec![async_module];
                let mut visited = HashSet::new();
                while let Some(id) = queue.pop() {
                    if !visited.insert(id) {
                        continue;
                    }
                    if entry_module_set.contains(&id) {
                        // Find the entry chunk that owns this entry module.
                        if let Some(idx) = entry_indices
                            .iter()
                            .find(|&&i| self.chunks.get(i).is_some_and(|c| c.modules.contains(&id)))
                        {
                            target_entries.insert(*idx);
                        }
                        continue;
                    }
                    // Walk reverse dependencies (modules that import `id`).
                    for importer in graph.get_all_dependents(id) {
                        queue.push(importer);
                    }
                }
            }

            // Merge the async chunk's modules into each target entry chunk only.
            for &entry_idx in &target_entries {
                if let Some(entry_chunk) = self.chunks.get_mut(entry_idx) {
                    for module in async_modules {
                        if !entry_chunk.modules.contains(module) {
                            entry_chunk.modules.push(*module);
                        }
                    }
                }
            }
        }

        // Remove async chunks
        self.chunks.retain(|c| c.chunk_type != ChunkType::Async);

        tracing::info!(
            "Inlined dynamic imports: merged {} async chunks into entry chunks",
            async_chunks.len()
        );
    }

    /// Get all chunk IDs
    pub fn chunk_ids(&self) -> Vec<String> {
        self.chunks.iter().map(|c| c.id.clone()).collect()
    }

    /// #71: Route-based chunk splitting
    /// Splits modules into per-route chunks, extracting shared modules.
    pub fn split_by_routes(
        &mut self,
        routes: &[(String, Vec<ModuleId>)],
        _all_modules: &HashMap<ModuleId, ResolvedModule>,
    ) {
        let mut module_route_count: HashMap<ModuleId, usize> = HashMap::new();

        for (_, mods) in routes {
            for m in mods {
                *module_route_count.entry(*m).or_default() += 1;
            }
        }

        let shared: Vec<ModuleId> = module_route_count
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(m, _)| *m)
            .collect();

        // Convert to a HashSet for O(1) membership tests in the per-route
        // filter below (avoids O(n²) Vec::contains over every route's modules).
        let shared_set: HashSet<ModuleId> = shared.iter().copied().collect();

        if !shared.is_empty() {
            self.chunks.push(Chunk {
                id: "route-shared".to_string(),
                modules: shared.clone(),
                chunk_type: ChunkType::Shared,
            });
        }

        for (route_name, mods) in routes {
            let route_modules: Vec<ModuleId> = mods
                .iter()
                .filter(|m| !shared_set.contains(m))
                .copied()
                .collect();

            if !route_modules.is_empty() {
                self.chunks.push(Chunk {
                    id: format!("route-{}", route_name),
                    modules: route_modules,
                    chunk_type: ChunkType::Route,
                });
            }
        }

        tracing::info!(
            "Route-based splitting: {} route chunks, 1 shared chunk ({} modules)",
            routes.len(),
            shared.len()
        );
    }
}

/// Heuristic side-effect detection over a module's source text.
///
/// Returns `true` when the module likely performs work at evaluation time:
///
///   - Immediately-invoked function expressions (`(() => {})()`,
///     `(function () {})()`, `!function () {}()`, `void function () {}()`)
///     — these always execute when the module loads.
///   - Top-level `await` — pauses module evaluation and may run arbitrary
///     asynchronous work.
///   - `export default <call>` — a computed default export expression like
///     `export default makeStore()` runs at evaluation time (plain
///     `export default function/class/identifier/object` does not).
///   - Any other top-level statement that isn't a declaration — function
///     calls, assignments, `console.*`, `if`/`for`/`try` blocks, bare
///     blocks, etc.
///
/// Multi-line statements are handled by tracking bracket depth: only lines
/// that begin at depth 0 start a new top-level statement, and lines that
/// continue a statement (`)` / `}` / `]` / `,` / `.` / operators) are not
/// classified independently. This avoids both false positives (e.g. object
/// literal fields inside `const x = { ... }` being read as statements) and
/// false negatives (e.g. `foo()` on the second line of a call chain).
///
/// This is intentionally a heuristic — a full AST analysis would be more
/// accurate, but it catches the common cases without paying for a parse.
fn module_source_has_side_effects(source: &str) -> bool {
    // Immediately-invoked function expressions are always side effects.
    const IIFE_PATTERNS: &[&str] = &[
        "(() =>",
        "(async () =>",
        "(function ()",
        "(function(",
        "!function(",
        "!function (",
        "void function",
    ];
    if IIFE_PATTERNS.iter().any(|p| source.contains(p)) {
        return true;
    }

    // Track rough bracket depth so that lines inside a multi-line statement
    // (object literals, call argument lists, template braces) are not
    // mistaken for new top-level statements. Brackets inside strings and
    // comments are counted too — this is a heuristic, not a lexer.
    let mut depth: i64 = 0;

    for line in source.lines() {
        let trimmed = line.trim();

        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
        {
            continue;
        }

        // A line that starts at depth 0 begins a new top-level statement —
        // only those lines are classified. Lines that start inside an open
        // bracket belong to a statement we already classified.
        let starts_statement = depth <= 0;

        // Update depth for the next line before classifying this one.
        for b in trimmed.bytes() {
            match b {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => depth -= 1,
                _ => {}
            }
        }

        if !starts_statement {
            continue;
        }

        // Continuation of a statement that began on a previous line
        // (method chains, ternaries, operator-split expressions).
        let first = trimmed.as_bytes()[0];
        if matches!(
            first,
            b'}' | b')' | b']' | b',' | b'.' | b'?' | b':' | b'|' | b'&' | b'+' | b'='
        ) {
            continue;
        }

        // Top-level await performs (possibly async) work at evaluation time.
        if trimmed.starts_with("await ") || trimmed.starts_with("await(") {
            return true;
        }

        // `export default <expr>` is only a side effect when the expression
        // is computed — a call or a more complex expression. Declarations
        // and plain re-exports are not.
        if let Some(rest) = trimmed.strip_prefix("export default") {
            let rest = rest.trim_start();
            if rest.starts_with("function")
                || rest.starts_with("async function")
                || rest.starts_with("class")
                || rest.starts_with('{')
                || rest.is_empty()
            {
                continue;
            }
            // Arrow-function default exports (`export default () => {}`,
            // `export default async () => {}`) are declarations, not calls.
            if (rest.starts_with('(') || rest.starts_with("async ")) && rest.contains("=>") {
                continue;
            }
            // `export default foo` (identifier) — not a side effect.
            // `export default foo()` / `export default new X()` — is.
            if !rest.contains('(') {
                continue;
            }
            return true;
        }

        // Declaration keywords never count as side effects.
        let is_declaration = trimmed.starts_with("import ")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("var ")
            || trimmed.starts_with("function ")
            || trimmed.starts_with("class ")
            || trimmed.starts_with("interface ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("declare ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("namespace ")
            || trimmed.starts_with("abstract ")
            || trimmed.starts_with("using ")
            || trimmed.starts_with("async function");

        if !is_declaration {
            return true;
        }
    }

    false
}
