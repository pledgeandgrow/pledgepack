// Serializable module graph for persistent storage and incremental rebuilds.
//
// The persistent module graph is serialized to disk between builds,
// enabling incremental rebuilds by comparing content hashes of the
// current build against the previous one. Only changed modules and
// their dependents are re-transformed.

use crate::module::{ModuleId, ModuleKind};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Serializable representation of a single module in the graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleNode {
    pub id: ModuleId,
    pub path: PathBuf,
    pub kind: String,
    pub content_hash: u64,
    /// Direct dependency module IDs
    pub dependencies: Vec<ModuleId>,
    /// Dynamic import module IDs
    pub dynamic_dependencies: Vec<ModuleId>,
}

/// Serializable representation of the entire module graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableModuleGraph {
    /// All modules keyed by ModuleId
    pub modules: HashMap<ModuleId, ModuleNode>,
    /// Entry point module IDs
    pub entry_modules: Vec<ModuleId>,
    /// Reverse dependency map: module ID → modules that depend on it
    pub reverse_deps: HashMap<ModuleId, Vec<ModuleId>>,
    /// Build timestamp
    pub built_at: u64,
    /// Git tree hash at build time (for fast cache invalidation)
    #[serde(default)]
    pub git_tree_hash: Option<String>,
}

impl SerializableModuleGraph {
    pub fn new() -> Self {
        Self {
            modules: HashMap::new(),
            entry_modules: Vec::new(),
            reverse_deps: HashMap::new(),
            built_at: 0,
            git_tree_hash: None,
        }
    }

    /// Add a module to the graph
    pub fn add_module(&mut self, id: ModuleId, path: PathBuf, kind: ModuleKind, content_hash: u64) {
        let kind_str = match kind {
            ModuleKind::Tsx => "tsx",
            ModuleKind::TypeScript => "ts",
            ModuleKind::Jsx => "jsx",
            ModuleKind::JavaScript => "js",
            ModuleKind::Css => "css",
            ModuleKind::Json => "json",
            ModuleKind::Wasm => "wasm",
            ModuleKind::Vue => "vue",
            ModuleKind::Svelte => "svelte",
            ModuleKind::Astro => "astro",
            ModuleKind::Worker => "worker",
            ModuleKind::SharedWorker => "sharedworker",
            ModuleKind::WebComponent => "webcomponent",
            ModuleKind::Asset => "asset",
            ModuleKind::Mdx => "mdx",
            ModuleKind::Graphql => "graphql",
            ModuleKind::Yaml => "yaml",
            ModuleKind::Csv => "csv",
            ModuleKind::Tsv => "tsv",
            ModuleKind::Sass => "sass",
            ModuleKind::Toml => "toml",
            ModuleKind::Shader => "shader",
            ModuleKind::Psx => "psx",
            ModuleKind::Ps => "ps",
            ModuleKind::Unknown => "unknown",
        };

        self.modules.insert(
            id,
            ModuleNode {
                id,
                path,
                kind: kind_str.to_string(),
                content_hash,
                dependencies: Vec::new(),
                dynamic_dependencies: Vec::new(),
            },
        );
    }

    /// Add a static dependency edge.
    ///
    /// Detects potential circular dependencies: if `to` can already reach `from`
    /// through the existing dependency graph, adding this edge would create a
    /// cycle. The edge is still added (some frameworks intentionally use
    /// circular imports), but a warning is logged so the issue is visible.
    pub fn add_dependency(&mut self, from: ModuleId, to: ModuleId) {
        if from != to && self.can_reach(to, from) {
            warn!("Circular dependency detected: {:?} -> {:?}", from, to);
        }
        if let Some(module) = self.modules.get_mut(&from)
            && !module.dependencies.contains(&to)
        {
            module.dependencies.push(to);
        }
        // Dedup the reverse edge too — the forward edge is deduped above,
        // but add_dependency can be called with the same (from, to) pair
        // multiple times (e.g. re-resolution during incremental rebuilds),
        // which would otherwise leave duplicate entries in reverse_deps.
        let reverse = self.reverse_deps.entry(to).or_default();
        if !reverse.contains(&from) {
            reverse.push(from);
        }
    }

    /// Add a dynamic import edge.
    ///
    /// Like `add_dependency`, this checks for cycles before adding the edge.
    /// Dynamic import cycles are less problematic (they resolve at runtime),
    /// but are still logged for visibility.
    pub fn add_dynamic_dependency(&mut self, from: ModuleId, to: ModuleId) {
        if from != to && self.can_reach(to, from) {
            warn!(
                "Circular dynamic dependency detected: {:?} -> {:?}",
                from, to
            );
        }
        if let Some(module) = self.modules.get_mut(&from)
            && !module.dynamic_dependencies.contains(&to)
        {
            module.dynamic_dependencies.push(to);
        }
        // Dedup the reverse edge (see add_dependency).
        let reverse = self.reverse_deps.entry(to).or_default();
        if !reverse.contains(&from) {
            reverse.push(from);
        }
    }

    /// Check whether `from` can reach `to` by following dependency edges.
    ///
    /// Traverses both static and dynamic dependency edges — a cycle formed
    /// through a dynamic import is still a cycle worth reporting. Uses an
    /// iterative DFS with a visited set to avoid infinite loops on graphs
    /// that already contain cycles. Returns `true` if a path exists.
    fn can_reach(&self, from: ModuleId, to: ModuleId) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![from];
        while let Some(current) = stack.pop() {
            if current == to {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(module) = self.modules.get(&current) {
                stack.extend(module.dependencies.iter().copied());
                // Also follow dynamic dependencies — a static+dynamic
                // mixed cycle is still a cycle.
                stack.extend(module.dynamic_dependencies.iter().copied());
            }
        }
        false
    }

    /// Set entry modules
    pub fn set_entries(&mut self, entries: Vec<ModuleId>) {
        self.entry_modules = entries;
    }

    /// Find all modules that changed by comparing content hashes
    /// with a previous graph snapshot
    pub fn find_changed_modules(&self, previous: &SerializableModuleGraph) -> HashSet<ModuleId> {
        let mut changed: HashSet<ModuleId> = HashSet::new();

        for (id, module) in &self.modules {
            match previous.modules.get(id) {
                Some(prev_module) => {
                    if module.content_hash != prev_module.content_hash {
                        debug!(
                            "Changed module: {:?} (hash {} → {})",
                            module.path, prev_module.content_hash, module.content_hash
                        );
                        changed.insert(*id);
                    }
                }
                None => {
                    debug!("New module: {:?}", module.path);
                    changed.insert(*id);
                }
            }
        }

        // Also mark modules that were removed (their dependents need rebuild)
        for id in previous.modules.keys() {
            if !self.modules.contains_key(id)
                && let Some(rev_deps) = previous.reverse_deps.get(id)
            {
                for dep_id in rev_deps {
                    if self.modules.contains_key(dep_id) {
                        changed.insert(*dep_id);
                    }
                }
            }
        }

        changed
    }

    /// Compute the transitive closure of dependents for a set of changed modules.
    /// Returns all modules that need to be re-transformed.
    pub fn compute_affected(&self, changed: &HashSet<ModuleId>) -> HashSet<ModuleId> {
        let mut affected = changed.clone();
        let mut queue: Vec<ModuleId> = changed.iter().copied().collect();

        while let Some(id) = queue.pop() {
            if let Some(rev_deps) = self.reverse_deps.get(&id) {
                for dep_id in rev_deps {
                    if affected.insert(*dep_id) {
                        queue.push(*dep_id);
                    }
                }
            }
        }

        affected
    }

    /// Get all modules that are NOT in the affected set (can be skipped)
    pub fn compute_unchanged(&self, affected: &HashSet<ModuleId>) -> Vec<ModuleId> {
        self.modules
            .keys()
            .filter(|id| !affected.contains(id))
            .copied()
            .collect()
    }

    /// Serialize to disk using bincode
    pub fn save_to_disk(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = bincode::serde::encode_to_vec(self, bincode::config::standard())?;
        std::fs::write(path, data)?;
        info!("Module graph saved: {} modules", self.modules.len());
        Ok(())
    }

    /// Load from disk using bincode with memory-mapped I/O for large graphs
    pub fn load_from_disk(path: &PathBuf) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let metadata = file.metadata()?;

        // Use memmap2 for zero-copy reads of large module graphs
        let data: Vec<u8> = if metadata.len() > 4096 {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            mmap.as_ref().to_vec()
        } else {
            std::fs::read(path)?
        };

        let (graph, _) = bincode::serde::decode_from_slice::<SerializableModuleGraph, _>(
            &data,
            bincode::config::standard(),
        )?;
        info!("Module graph loaded: {} modules", graph.modules.len());
        Ok(graph)
    }

    /// Check if a graph snapshot exists on disk
    pub fn exists_on_disk(path: &Path) -> bool {
        path.exists()
    }
}

impl Default for SerializableModuleGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_changed_modules() {
        let mut prev = SerializableModuleGraph::new();
        prev.add_module(0, PathBuf::from("/src/a.ts"), ModuleKind::TypeScript, 100);
        prev.add_module(1, PathBuf::from("/src/b.ts"), ModuleKind::TypeScript, 200);
        prev.add_dependency(0, 1);

        let mut curr = SerializableModuleGraph::new();
        curr.add_module(0, PathBuf::from("/src/a.ts"), ModuleKind::TypeScript, 100);
        curr.add_module(1, PathBuf::from("/src/b.ts"), ModuleKind::TypeScript, 999);

        let changed = curr.find_changed_modules(&prev);
        assert!(changed.contains(&1));
        assert!(!changed.contains(&0));
    }

    #[test]
    fn test_affected_dependents() {
        let mut graph = SerializableModuleGraph::new();
        graph.add_module(0, PathBuf::from("/src/a.ts"), ModuleKind::TypeScript, 100);
        graph.add_module(1, PathBuf::from("/src/b.ts"), ModuleKind::TypeScript, 200);
        graph.add_module(2, PathBuf::from("/src/c.ts"), ModuleKind::TypeScript, 300);
        graph.add_dependency(0, 1);
        graph.add_dependency(1, 2);

        let mut changed = HashSet::new();
        changed.insert(2);

        let affected = graph.compute_affected(&changed);
        assert!(affected.contains(&2));
        assert!(affected.contains(&1));
        assert!(affected.contains(&0));
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("pledgepack_graph_test");
        let path = dir.join("module_graph.bin");

        let mut graph = SerializableModuleGraph::new();
        graph.add_module(0, PathBuf::from("/src/a.ts"), ModuleKind::TypeScript, 100);
        graph.add_module(1, PathBuf::from("/src/b.ts"), ModuleKind::TypeScript, 200);
        graph.add_dependency(0, 1);
        graph.set_entries(vec![0]);

        graph.save_to_disk(&path).unwrap();
        let loaded = SerializableModuleGraph::load_from_disk(&path).unwrap();

        assert_eq!(loaded.modules.len(), 2);
        assert_eq!(loaded.entry_modules, vec![0]);
        assert!(loaded.reverse_deps.get(&1).unwrap().contains(&0));
    }
}
