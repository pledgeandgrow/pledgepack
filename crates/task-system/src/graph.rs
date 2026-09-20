// Dependency graph + aggregation graph for PledgePack's task system.
//
// Two parallel graph structures:
//
// 1. **Dependency graph**: records which tasks depend on which.
//    When a leaf task changes (different TaskId), we traverse the graph
//    to find transitively-dependent tasks. Edges are stored as adjacency lists.
//
// 2. **Aggregation graph**: a tree structure parallel to the dependency graph
//    that enables O(log n) sub-graph queries. Each aggregation node summarizes
//    the dirty/clean status of its subtree. When a leaf changes, we propagate
//    dirty status up the aggregation tree in O(log n).
//
// Both graphs are arena-friendly (contiguous arrays) for cache locality and
// future Zig arena integration. The current implementation uses Rust HashMaps
// and Vecs for simplicity; the Zig arena (native-sys/zig/graph.zig) will be
// wired in as the hot-path storage layer.

use crate::task::TaskId;
use dashmap::DashMap;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use tracing::debug;

/// The status of a task in the dependency graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TaskStatus {
    /// The task has been computed and its output is cached.
    Clean,
    /// The task's inputs have changed and it needs recomputation.
    Dirty,
    /// The task is currently being computed.
    Computing,
    /// The task computation failed.
    Error,
    /// The task has not been scheduled yet.
    Pending,
}

/// The dependency graph — records edges between tasks.
///
/// Edges: `dependents[task]` = set of tasks that depend on `task`.
/// Reverse edges: `dependencies[task]` = set of tasks that `task` depends on.
///
/// When a task's output changes (different output hash), we mark all tasks in
/// `dependents[task]` as dirty. This is the invalidation propagation.
///
/// Thread-safe via DashMap.
#[derive(Default)]
pub struct DependencyGraph {
    /// task → set of tasks that depend on it (reverse edges for invalidation)
    dependents: DashMap<TaskId, HashSet<TaskId>>,
    /// task → set of tasks it depends on (forward edges for traversal)
    dependencies: DashMap<TaskId, HashSet<TaskId>>,
    /// task → current status
    status: DashMap<TaskId, TaskStatus>,
}

impl DependencyGraph {
    /// Create an empty dependency graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a dependency edge: `parent` depends on `child`.
    ///
    /// This is called when a task is computed — the task's `dependencies` field
    /// (from `StoredOutput`) is used to register all edges.
    pub fn add_edge(&self, parent: TaskId, child: TaskId) {
        self.dependencies.entry(parent).or_default().insert(child);
        self.dependents.entry(child).or_default().insert(parent);
    }

    /// Record multiple dependency edges at once.
    pub fn add_edges(&self, parent: TaskId, children: &[TaskId]) {
        for &child in children {
            self.add_edge(parent, child);
        }
    }

    /// Get all tasks that depend on `task` (direct dependents).
    pub fn dependents(&self, task: &TaskId) -> HashSet<TaskId> {
        self.dependents
            .get(task)
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// Get all tasks that `task` depends on (direct dependencies).
    pub fn dependencies(&self, task: &TaskId) -> HashSet<TaskId> {
        self.dependencies
            .get(task)
            .map(|r| r.clone())
            .unwrap_or_default()
    }

    /// Get the status of a task.
    pub fn status(&self, task: &TaskId) -> TaskStatus {
        self.status
            .get(task)
            .map(|r| *r)
            .unwrap_or(TaskStatus::Pending)
    }

    /// Set the status of a task.
    pub fn set_status(&self, task: TaskId, status: TaskStatus) {
        self.status.insert(task, status);
    }

    /// Mark a task as dirty and propagate to all transitive dependents.
    ///
    /// This is the invalidation propagation: when a leaf task changes,
    /// all tasks that (transitively) depend on it become dirty.
    ///
    /// Returns the set of all tasks that were marked dirty.
    pub fn mark_dirty(&self, task: TaskId) -> HashSet<TaskId> {
        let mut dirty = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(task);

        while let Some(current) = queue.pop_front() {
            if dirty.contains(&current) {
                continue;
            }
            self.set_status(current, TaskStatus::Dirty);
            dirty.insert(current);

            // Propagate to dependents
            for dependent in self.dependents(&current) {
                if !dirty.contains(&dependent) {
                    queue.push_back(dependent);
                }
            }
        }

        debug!("Marked {} tasks dirty from {}", dirty.len(), task);
        dirty
    }

    /// Mark a task as clean (after successful computation).
    pub fn mark_clean(&self, task: TaskId) {
        self.set_status(task, TaskStatus::Clean);
    }

    /// Get all dirty tasks.
    pub fn dirty_tasks(&self) -> Vec<TaskId> {
        self.status
            .iter()
            .filter(|r| *r.value() == TaskStatus::Dirty)
            .map(|r| *r.key())
            .collect()
    }

    /// Get all tasks that are clean.
    pub fn clean_tasks(&self) -> Vec<TaskId> {
        self.status
            .iter()
            .filter(|r| *r.value() == TaskStatus::Clean)
            .map(|r| *r.key())
            .collect()
    }

    /// Get all tasks in the graph.
    pub fn all_tasks(&self) -> Vec<TaskId> {
        let mut tasks: HashSet<TaskId> = HashSet::new();
        for r in self.dependents.iter() {
            tasks.insert(*r.key());
            for dep in r.value() {
                tasks.insert(*dep);
            }
        }
        for r in self.dependencies.iter() {
            tasks.insert(*r.key());
            for dep in r.value() {
                tasks.insert(*dep);
            }
        }
        tasks.into_iter().collect()
    }

    /// Remove a task and all its edges from the graph.
    pub fn remove(&self, task: &TaskId) {
        // Remove from dependents map
        if let Some((_, deps)) = self.dependencies.remove(task) {
            for dep in &deps {
                if let Some(mut dependents) = self.dependents.get_mut(dep) {
                    dependents.remove(task);
                }
            }
        }
        // Remove from dependents map
        if let Some((_, dependents)) = self.dependents.remove(task) {
            for dep in &dependents {
                if let Some(mut deps) = self.dependencies.get_mut(dep) {
                    deps.remove(task);
                }
            }
        }
        self.status.remove(task);
    }

    /// Clear the entire graph.
    pub fn clear(&self) {
        self.dependents.clear();
        self.dependencies.clear();
        self.status.clear();
    }

    /// Number of tasks in the graph.
    pub fn len(&self) -> usize {
        self.all_tasks().len()
    }

    /// Is the graph empty?
    pub fn is_empty(&self) -> bool {
        self.dependents.is_empty() && self.dependencies.is_empty()
    }

    /// Find all root tasks (tasks with no dependents — top of the graph).
    pub fn roots(&self) -> Vec<TaskId> {
        let all: HashSet<TaskId> = self.all_tasks().into_iter().collect();
        let has_dependents: HashSet<TaskId> = self
            .dependents
            .iter()
            .filter(|r| !r.value().is_empty())
            .map(|r| *r.key())
            .collect();
        all.difference(&has_dependents).copied().collect()
    }

    /// Find all leaf tasks (tasks with no dependencies — bottom of the graph).
    pub fn leaves(&self) -> Vec<TaskId> {
        let all: HashSet<TaskId> = self.all_tasks().into_iter().collect();
        let has_deps: HashSet<TaskId> = self
            .dependencies
            .iter()
            .filter(|r| !r.value().is_empty())
            .map(|r| *r.key())
            .collect();
        all.difference(&has_deps).copied().collect()
    }
}

/// Backend-agnostic interface for the task dependency graph.
///
/// Implemented by both the Rust `DependencyGraph` (DashMap-backed) and the
/// Zig `ZigTaskGraph` (arena-backed, `zig-graph` feature). `TaskEngine`
/// stores `Box<dyn TaskGraphOps>` and picks the backend at construction —
/// Zig by default when the feature is compiled in, overridable with the
/// `PLEDGE_GRAPH_BACKEND=rust` environment variable for A/B benchmarking.
///
/// All methods take `&self`; backends provide their own synchronization
/// (DashMap internally, or a Mutex inside `ZigTaskGraph`).
pub trait TaskGraphOps: Send + Sync {
    /// Record a dependency edge: `parent` depends on `child`.
    fn add_edge(&self, parent: TaskId, child: TaskId);
    /// Record multiple dependency edges at once.
    fn add_edges(&self, parent: TaskId, children: &[TaskId]) {
        for &child in children {
            self.add_edge(parent, child);
        }
    }
    /// Tasks that depend on `task` (reverse edges).
    fn dependents(&self, task: &TaskId) -> HashSet<TaskId>;
    /// Tasks that `task` depends on (forward edges).
    fn dependencies(&self, task: &TaskId) -> HashSet<TaskId>;
    /// Current status of a task.
    fn status(&self, task: &TaskId) -> TaskStatus;
    /// Set the status of a task.
    fn set_status(&self, task: TaskId, status: TaskStatus);
    /// Mark `task` and all transitive dependents dirty; returns the dirtied set.
    fn mark_dirty(&self, task: TaskId) -> HashSet<TaskId>;
    /// Mark a task clean (after successful computation).
    fn mark_clean(&self, task: TaskId) {
        self.set_status(task, TaskStatus::Clean);
    }
    /// All tasks currently marked dirty.
    fn dirty_tasks(&self) -> Vec<TaskId>;
    /// All tasks currently marked clean.
    fn clean_tasks(&self) -> Vec<TaskId>;
    /// Every task known to the graph.
    fn all_tasks(&self) -> Vec<TaskId>;
    /// Remove all tasks and edges.
    fn clear(&self);
    /// Number of tasks in the graph.
    fn len(&self) -> usize;
    /// Whether the graph is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TaskGraphOps for DependencyGraph {
    fn add_edge(&self, parent: TaskId, child: TaskId) {
        DependencyGraph::add_edge(self, parent, child);
    }
    fn add_edges(&self, parent: TaskId, children: &[TaskId]) {
        DependencyGraph::add_edges(self, parent, children);
    }
    fn dependents(&self, task: &TaskId) -> HashSet<TaskId> {
        DependencyGraph::dependents(self, task)
    }
    fn dependencies(&self, task: &TaskId) -> HashSet<TaskId> {
        DependencyGraph::dependencies(self, task)
    }
    fn status(&self, task: &TaskId) -> TaskStatus {
        DependencyGraph::status(self, task)
    }
    fn set_status(&self, task: TaskId, status: TaskStatus) {
        DependencyGraph::set_status(self, task, status);
    }
    fn mark_dirty(&self, task: TaskId) -> HashSet<TaskId> {
        DependencyGraph::mark_dirty(self, task)
    }
    fn dirty_tasks(&self) -> Vec<TaskId> {
        DependencyGraph::dirty_tasks(self)
    }
    fn clean_tasks(&self) -> Vec<TaskId> {
        DependencyGraph::clean_tasks(self)
    }
    fn all_tasks(&self) -> Vec<TaskId> {
        DependencyGraph::all_tasks(self)
    }
    fn clear(&self) {
        DependencyGraph::clear(self);
    }
    fn len(&self) -> usize {
        DependencyGraph::len(self)
    }
    fn is_empty(&self) -> bool {
        DependencyGraph::is_empty(self)
    }
}

/// Construct the default dependency-graph backend.
///
/// With the `zig-graph` feature enabled (the default), this is the Zig
/// arena graph — 24-byte nodes, flat edge arrays, single-pass dirty
/// propagation. Set `PLEDGE_GRAPH_BACKEND=rust` to force the DashMap
/// backend for comparison or as a fallback.
pub fn default_dep_graph() -> Box<dyn TaskGraphOps> {
    #[cfg(feature = "zig-graph")]
    {
        if !matches!(std::env::var("PLEDGE_GRAPH_BACKEND").as_deref(), Ok("rust")) {
            return Box::new(crate::zig_graph::ZigTaskGraph::new());
        }
    }
    Box::new(DependencyGraph::new())
}

/// An aggregation node in the aggregation graph.
///
/// The aggregation graph is a tree (or forest) parallel to the dependency graph.
/// Each aggregation node summarizes the status of its subtree:
///   - Dirty count: how many tasks in this subtree are dirty
///   - Error count: how many tasks in this subtree have errors
///   - Total count: total tasks in this subtree
///
/// This enables O(log n) sub-graph queries: "is this subtree clean?" is a
/// single aggregation node lookup, not a full traversal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AggregationNode {
    /// The task this node represents.
    pub task_id: TaskId,
    /// Child aggregation nodes (subtree).
    pub children: Vec<TaskId>,
    /// Number of dirty tasks in this subtree.
    pub dirty_count: u32,
    /// Number of error tasks in this subtree.
    pub error_count: u32,
    /// Total tasks in this subtree (including this node).
    pub total_count: u32,
    /// G3.8: Total compute time in milliseconds for this subtree.
    pub total_compute_ms: u64,
    /// G3.8: Number of cached tasks in this subtree (avoided recomputation).
    pub cached_count: u32,
    /// G3.7: Whether this node has been compacted (children removed, summary only).
    pub compacted: bool,
}

impl AggregationNode {
    /// Create a fresh, clean aggregation node for a single task (no children yet).
    pub fn new(task_id: TaskId) -> Self {
        AggregationNode {
            task_id,
            children: Vec::new(),
            dirty_count: 0,
            error_count: 0,
            total_count: 1,
            total_compute_ms: 0,
            cached_count: 0,
            compacted: false,
        }
    }

    /// Is this subtree fully clean?
    pub fn is_clean(&self) -> bool {
        self.dirty_count == 0 && self.error_count == 0
    }

    /// Does this subtree have any dirty tasks?
    pub fn has_dirty(&self) -> bool {
        self.dirty_count > 0
    }

    /// Does this subtree have any errors?
    pub fn has_errors(&self) -> bool {
        self.error_count > 0
    }

    /// G3.8: Return a metrics summary for this aggregation node.
    ///
    /// Returns a human-readable string with task count, dirty count,
    /// error count, cached count, and total compute time.
    pub fn metrics_summary(&self) -> String {
        format!(
            "tasks={}, dirty={}, errors={}, cached={}, compute_ms={}",
            self.total_count,
            self.dirty_count,
            self.error_count,
            self.cached_count,
            self.total_compute_ms
        )
    }

    /// G3.8: Record compute time for this node.
    pub fn record_compute_time(&mut self, ms: u64) {
        self.total_compute_ms += ms;
    }

    /// G3.8: Mark this node as cached (avoided recomputation).
    pub fn mark_cached(&mut self) {
        self.cached_count += 1;
    }
}

/// The aggregation graph — a tree structure for O(log n) sub-graph queries.
///
/// Each task has an aggregation node. When a leaf task changes, we update its
/// aggregation node and propagate the dirty/clean delta up the tree in O(log n).
///
/// The aggregation graph is built from the dependency graph: each task's
/// aggregation children are its direct dependencies. Root tasks (no dependents)
/// are the roots of the aggregation forest.
pub struct AggregationGraph {
    nodes: DashMap<TaskId, AggregationNode>,
    /// G3.10: Set of tasks that have been queried — only these subtrees get aggregation nodes.
    queried: Mutex<HashSet<TaskId>>,
    /// Whether a cycle has been detected in the dependency graph. Once set,
    /// aggregation counts may be inaccurate for tasks involved in the cycle.
    has_cycle: std::sync::atomic::AtomicBool,
}

impl AggregationGraph {
    /// Create an empty aggregation graph.
    pub fn new() -> Self {
        AggregationGraph {
            nodes: DashMap::new(),
            queried: Mutex::new(HashSet::new()),
            has_cycle: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Build the aggregation graph from a dependency graph.
    ///
    /// Each task's aggregation children are its direct dependencies.
    /// Aggregation counts are computed bottom-up.
    pub fn build_from(&self, dep_graph: &dyn TaskGraphOps) {
        let all_tasks = dep_graph.all_tasks();
        for &task in &all_tasks {
            let deps = dep_graph.dependencies(&task);
            let mut node = AggregationNode::new(task);
            node.children = deps.iter().copied().collect();
            self.nodes.insert(task, node);
        }
        // Compute counts bottom-up (topological order)
        self.recompute_counts(dep_graph);
    }

    /// G3.10: Query a subtree lazily — only creates aggregation nodes for the
    /// queried task and its transitive dependencies.
    ///
    /// Unlike `build_from`, which eagerly creates nodes for all tasks, this
    /// method only materializes nodes that are actually needed for the query.
    /// Subsequent queries for the same task reuse existing nodes.
    ///
    /// Returns the aggregation node for the queried task, or None if the task
    /// is not in the dependency graph.
    pub fn query_subtree(
        &self,
        task: TaskId,
        dep_graph: &dyn TaskGraphOps,
    ) -> Option<AggregationNode> {
        // Mark as queried
        {
            let mut queried = self
                .queried
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queried.contains(&task) {
                // Already queried — return existing node
                return self.nodes.get(&task).map(|r| r.value().clone());
            }
            queried.insert(task);
        }

        // Lazily create nodes for this task and all its transitive deps
        let mut to_create: Vec<TaskId> = vec![task];
        let mut visited: HashSet<TaskId> = HashSet::new();

        while let Some(t) = to_create.pop() {
            if visited.contains(&t) {
                continue;
            }
            visited.insert(t);

            if !self.nodes.contains_key(&t) {
                let deps = dep_graph.dependencies(&t);
                let mut node = AggregationNode::new(t);
                node.children = deps.iter().copied().collect();
                self.nodes.insert(t, node);
            }

            // Add deps to create
            for dep in dep_graph.dependencies(&t) {
                if !visited.contains(&dep) {
                    to_create.push(dep);
                }
            }
        }

        // Recompute counts for the newly created subtree (bottom-up)
        // Process in topological order within the visited set
        let visited_set: HashSet<TaskId> = visited.clone();
        let mut processed: HashSet<TaskId> = HashSet::new();
        let mut remaining: Vec<TaskId> = visited.into_iter().collect();

        let mut iterations = 0;
        let max_iterations = remaining.len() + 1;
        while !remaining.is_empty() && iterations < max_iterations {
            iterations += 1;
            let mut next_remaining = Vec::new();
            for &t in &remaining {
                let deps = dep_graph.dependencies(&t);
                let all_deps_ready = deps
                    .iter()
                    .all(|d| processed.contains(d) || !visited_set.contains(d));
                if all_deps_ready {
                    self.update_subtree_count(t, dep_graph);
                    processed.insert(t);
                } else {
                    next_remaining.push(t);
                }
            }
            remaining = next_remaining;
        }

        // Cycle detection: if tasks remain unprocessed, a cycle exists in the
        // queried subtree. Aggregation counts for these tasks will be stale.
        if !remaining.is_empty() {
            self.has_cycle
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                "Cycle detected in aggregation graph (query_subtree): {} tasks remain unprocessed",
                remaining.len()
            );
        }

        self.nodes.get(&task).map(|r| r.value().clone())
    }

    /// G3.10: Returns the number of tasks that have been queried.
    pub fn queried_count(&self) -> usize {
        self.queried
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// G3.10: Returns true if a task has been queried (its subtree is materialized).
    pub fn is_queried(&self, task: &TaskId) -> bool {
        self.queried
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(task)
    }

    /// Returns true if a cycle has been detected in the dependency graph.
    ///
    /// When a cycle is detected, aggregation counts for tasks involved in the
    /// cycle may be inaccurate. The flag is set once and never cleared — if a
    /// cycle was ever encountered, this will return true.
    pub fn has_cycle(&self) -> bool {
        self.has_cycle.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// G3.9: Incrementally rebuild only the affected aggregation nodes.
    ///
    /// When new tasks are added or existing tasks change, this method only
    /// updates the aggregation nodes for the affected tasks and their
    /// transitive dependents — not the entire graph.
    ///
    /// `affected_tasks` are the tasks whose dependencies or status changed.
    /// The method finds all transitive dependents and recomputes their
    /// aggregation counts in topological order.
    pub fn incremental_rebuild(&self, affected_tasks: &[TaskId], dep_graph: &dyn TaskGraphOps) {
        // Collect all affected tasks + their transitive dependents
        let mut to_rebuild: HashSet<TaskId> = HashSet::new();
        let mut queue: Vec<TaskId> = affected_tasks.to_vec();
        while let Some(task) = queue.pop() {
            if to_rebuild.contains(&task) {
                continue;
            }
            to_rebuild.insert(task);
            // Add all dependents (parents in the aggregation tree)
            for dep in dep_graph.dependents(&task) {
                if !to_rebuild.contains(&dep) {
                    queue.push(dep);
                }
            }
        }

        // Also add new tasks that don't have aggregation nodes yet
        for &task in &to_rebuild {
            if !self.nodes.contains_key(&task) {
                let deps = dep_graph.dependencies(&task);
                let mut node = AggregationNode::new(task);
                node.children = deps.iter().copied().collect();
                self.nodes.insert(task, node);
            }
        }

        // Topological sort of affected tasks (leaves first)
        let mut processed: HashSet<TaskId> = HashSet::new();
        let rebuild_set: HashSet<TaskId> = to_rebuild.iter().copied().collect();
        let mut remaining: Vec<TaskId> = to_rebuild.into_iter().collect();
        let mut iterations = 0;
        let max_iterations = remaining.len() + 1;
        while !remaining.is_empty() && iterations < max_iterations {
            iterations += 1;
            let mut next_remaining = Vec::new();
            for &task in &remaining {
                let deps = dep_graph.dependencies(&task);
                // A dep is ready if it's processed OR not in the rebuild set
                // (deps outside the rebuild set still have valid counts from previous build)
                let all_deps_ready = deps
                    .iter()
                    .all(|d| processed.contains(d) || !rebuild_set.contains(d));
                if all_deps_ready {
                    self.update_subtree_count(task, dep_graph);
                    processed.insert(task);
                } else {
                    next_remaining.push(task);
                }
            }
            remaining = next_remaining;
        }

        // Cycle detection: if tasks remain unprocessed, a cycle exists among
        // the affected tasks. Aggregation counts for these tasks will be stale.
        if !remaining.is_empty() {
            self.has_cycle
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                "Cycle detected in aggregation graph (incremental_rebuild): {} tasks remain unprocessed",
                remaining.len()
            );
        }
    }

    /// Recompute aggregation counts from the dependency graph.
    ///
    /// This is called after the dependency graph changes. Uses a bottom-up
    /// traversal (topological order from leaves to roots) to ensure child
    /// counts are computed before parent counts.
    fn recompute_counts(&self, dep_graph: &dyn TaskGraphOps) {
        let all_tasks = dep_graph.all_tasks();
        // Topological sort: process leaves first, then parents.
        // We use a simple approach: repeatedly find tasks whose dependencies
        // have all been processed.
        let mut processed: HashSet<TaskId> = HashSet::new();
        let mut remaining: Vec<TaskId> = all_tasks.clone();

        let mut iterations = 0;
        let max_iterations = remaining.len() + 1;
        while !remaining.is_empty() && iterations < max_iterations {
            iterations += 1;
            let mut next_remaining = Vec::new();
            for &task in &remaining {
                let deps = dep_graph.dependencies(&task);
                // Check if all deps are processed (or don't exist in the graph)
                let all_deps_processed = deps
                    .iter()
                    .all(|d| processed.contains(d) || !all_tasks.contains(d));
                if all_deps_processed {
                    self.update_subtree_count(task, dep_graph);
                    processed.insert(task);
                } else {
                    next_remaining.push(task);
                }
            }
            remaining = next_remaining;
        }

        // Cycle detection: if tasks remain unprocessed, a cycle exists in the
        // dependency graph. Aggregation counts for these tasks will be stale.
        if !remaining.is_empty() {
            self.has_cycle
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(
                "Cycle detected in aggregation graph: {} tasks remain unprocessed",
                remaining.len()
            );
        }
    }

    /// Update the aggregation counts for a single task's subtree.
    fn update_subtree_count(&self, task: TaskId, dep_graph: &dyn TaskGraphOps) {
        let deps = dep_graph.dependencies(&task);
        let mut dirty_count = 0u32;
        let mut error_count = 0u32;
        let mut total_count = 1u32; // this task

        for dep in &deps {
            // Recursively get child counts
            if let Some(child_node) = self.nodes.get(dep) {
                dirty_count += child_node.dirty_count;
                error_count += child_node.error_count;
                total_count += child_node.total_count;
            }
        }

        // Add this task's own status
        match dep_graph.status(&task) {
            TaskStatus::Dirty => dirty_count += 1,
            TaskStatus::Error => error_count += 1,
            _ => {}
        }

        if let Some(mut node) = self.nodes.get_mut(&task) {
            node.dirty_count = dirty_count;
            node.error_count = error_count;
            node.total_count = total_count;
        }
    }

    /// Mark a task as dirty and propagate the dirty delta up the aggregation tree.
    ///
    /// This is O(log n) — we only update the path from the leaf to the root,
    /// not the entire tree.
    pub fn mark_dirty(&self, task: TaskId, dep_graph: &dyn TaskGraphOps) {
        // Update this node
        if let Some(mut node) = self.nodes.get_mut(&task) {
            let was_dirty = node.dirty_count > 0;
            if !was_dirty {
                node.dirty_count += 1;
            }
        }

        // Propagate to dependents (up the tree)
        let dependents = dep_graph.dependents(&task);
        for dep in dependents {
            self.mark_dirty(dep, dep_graph);
        }
    }

    /// Mark a task as clean and propagate the clean delta up the aggregation tree.
    pub fn mark_clean(&self, task: TaskId, dep_graph: &dyn TaskGraphOps) {
        if let Some(mut node) = self.nodes.get_mut(&task)
            && node.dirty_count > 0
        {
            node.dirty_count = node.dirty_count.saturating_sub(1);
        }

        let dependents = dep_graph.dependents(&task);
        for dep in dependents {
            self.mark_clean(dep, dep_graph);
        }
    }

    /// G3.7: Compact a clean subtree into a single summary node.
    ///
    /// When a sub-graph is fully clean and computed, removes all child
    /// aggregation nodes and keeps only the root node with aggregated counts.
    /// This reduces memory usage for large clean sub-graphs.
    ///
    /// Returns the number of nodes removed (compacted away).
    /// If the subtree is not clean, returns 0 and does nothing.
    pub fn compact(&self, task: TaskId, dep_graph: &dyn TaskGraphOps) -> usize {
        // Only compact if the subtree is clean
        let is_clean = self.nodes.get(&task).map(|r| r.is_clean()).unwrap_or(false);
        if !is_clean {
            return 0;
        }

        // Already compacted — skip
        let already_compacted = self.nodes.get(&task).map(|r| r.compacted).unwrap_or(false);
        if already_compacted {
            return 0;
        }

        // Collect all transitive dependencies (children in aggregation tree)
        let mut to_remove: HashSet<TaskId> = HashSet::new();
        let mut queue: Vec<TaskId> = dep_graph.dependencies(&task).into_iter().collect();
        while let Some(t) = queue.pop() {
            if to_remove.contains(&t) {
                continue;
            }
            to_remove.insert(t);
            for dep in dep_graph.dependencies(&t) {
                if !to_remove.contains(&dep) {
                    queue.push(dep);
                }
            }
        }

        let removed = to_remove.len();

        // Remove child nodes
        for t in &to_remove {
            self.nodes.remove(t);
        }

        // Mark the root as compacted and clear children list
        if let Some(mut node) = self.nodes.get_mut(&task) {
            node.compacted = true;
            node.children.clear();
        }

        removed
    }

    /// G3.7: Compact all clean subtrees in the aggregation graph.
    ///
    /// Finds all root tasks (tasks with no dependents) and compacts their
    /// clean subtrees. Returns the total number of nodes removed.
    pub fn compact_all(&self, dep_graph: &dyn TaskGraphOps) -> usize {
        let all_tasks = dep_graph.all_tasks();
        let roots: Vec<TaskId> = all_tasks
            .iter()
            .filter(|t| dep_graph.dependents(t).is_empty())
            .copied()
            .collect();

        let mut total_removed = 0;
        for root in roots {
            total_removed += self.compact(root, dep_graph);
        }
        total_removed
    }

    /// G3.7: Returns the number of compacted nodes in the graph.
    pub fn compacted_count(&self) -> usize {
        self.nodes.iter().filter(|r| r.value().compacted).count()
    }

    /// Get the aggregation node for a task.
    pub fn get(&self, task: &TaskId) -> Option<AggregationNode> {
        self.nodes.get(task).map(|r| r.clone())
    }

    /// Is the subtree rooted at `task` fully clean?
    pub fn is_subtree_clean(&self, task: &TaskId) -> bool {
        self.nodes.get(task).map(|r| r.is_clean()).unwrap_or(true)
    }

    /// How many dirty tasks are in the subtree rooted at `task`?
    pub fn subtree_dirty_count(&self, task: &TaskId) -> u32 {
        self.nodes.get(task).map(|r| r.dirty_count).unwrap_or(0)
    }

    /// How many tasks are in the subtree rooted at `task`?
    pub fn subtree_total_count(&self, task: &TaskId) -> u32 {
        self.nodes.get(task).map(|r| r.total_count).unwrap_or(0)
    }

    /// Clear the aggregation graph.
    pub fn clear(&self) {
        self.nodes.clear();
    }

    /// G3.8: Get the metrics summary string for a task's aggregation node.
    pub fn get_metrics(&self, task: &TaskId) -> Option<String> {
        self.nodes.get(task).map(|r| r.metrics_summary())
    }

    /// G3.8: Record compute time for a task's aggregation node.
    pub fn record_compute_time(&self, task: &TaskId, ms: u64) {
        if let Some(mut node) = self.nodes.get_mut(task) {
            node.record_compute_time(ms);
        }
    }

    /// G3.8: Mark a task as cached in its aggregation node.
    pub fn mark_cached(&self, task: &TaskId) {
        if let Some(mut node) = self.nodes.get_mut(task) {
            node.mark_cached();
        }
    }

    /// G3.8: Get total compute time for a subtree.
    pub fn subtree_compute_ms(&self, task: &TaskId) -> u64 {
        self.nodes
            .get(task)
            .map(|r| r.total_compute_ms)
            .unwrap_or(0)
    }

    /// G3.8: Get cached count for a subtree.
    pub fn subtree_cached_count(&self, task: &TaskId) -> u32 {
        self.nodes.get(task).map(|r| r.cached_count).unwrap_or(0)
    }

    /// G3.13: Visualize the aggregation graph as an SVG string.
    ///
    /// Each node is rendered as a rectangle with its short hex ID, total count,
    /// and dirty count. Clean nodes are green, dirty nodes are red.
    pub fn visualize_svg(&self) -> String {
        let all_nodes: Vec<(TaskId, AggregationNode)> = self
            .nodes
            .iter()
            .map(|r| (*r.key(), r.value().clone()))
            .collect();

        let node_count = all_nodes.len();
        let width = 800;
        let node_height = 60;
        let node_width = 180;
        let padding = 20;
        let height = (node_count as u32 + 1) * (node_height + padding);

        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\">\n",
            width, height
        );
        svg.push_str("  <style>\n");
        svg.push_str("    .agg-node {{ font-family: monospace; font-size: 12px; }}\n");
        svg.push_str("    .agg-label {{ fill: #333; }}\n");
        svg.push_str("    .agg-metrics {{ fill: #666; font-size: 10px; }}\n");
        svg.push_str("  </style>\n");

        // Build child → parent edges for drawing
        let mut child_to_parents: HashMap<TaskId, Vec<TaskId>> = HashMap::new();
        for (id, node) in &all_nodes {
            for child in &node.children {
                child_to_parents.entry(*child).or_default().push(*id);
            }
        }

        // Position nodes in a simple vertical layout
        let mut positions: HashMap<TaskId, (u32, u32)> = HashMap::new();
        for (i, (id, _)) in all_nodes.iter().enumerate() {
            let y = (i as u32 + 1) * (node_height + padding);
            let x = padding + 200;
            positions.insert(*id, (x, y));
        }

        // Draw edges
        for (id, node) in &all_nodes {
            if let Some(&(x, y)) = positions.get(id) {
                for child in &node.children {
                    if let Some(&(cx, cy)) = positions.get(child) {
                        svg.push_str(&format!(
                            "  <line x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#999\" stroke-width=\"1\"/>\n",
                            cx + node_width / 2,
                            cy + node_height,
                            x + node_width / 2,
                            y
                        ));
                    }
                }
            }
        }

        // Draw nodes
        for (id, node) in &all_nodes {
            if let Some(&(x, y)) = positions.get(id) {
                let color = if node.dirty_count > 0 {
                    "#FFB6C1"
                } else {
                    "#90EE90"
                };
                let label = id.short_hex();
                svg.push_str(&format!(
                    "  <rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"{}\" stroke=\"#333\" rx=\"4\"/>\n",
                    x, y, node_width, node_height, color
                ));
                svg.push_str(&format!(
                    "  <text x=\"{}\" y=\"{}\" class=\"agg-node agg-label\">{}</text>\n",
                    x + 8,
                    y + 18,
                    label
                ));
                svg.push_str(&format!(
                    "  <text x=\"{}\" y=\"{}\" class=\"agg-node agg-metrics\">total={} dirty={} errors={}</text>\n",
                    x + 8,
                    y + 36,
                    node.total_count,
                    node.dirty_count,
                    node.error_count
                ));
                if node.total_compute_ms > 0 {
                    svg.push_str(&format!(
                        "  <text x=\"{}\" y=\"{}\" class=\"agg-node agg-metrics\">compute={}ms cached={}</text>\n",
                        x + 8,
                        y + 52,
                        node.total_compute_ms,
                        node.cached_count
                    ));
                }
            }
        }

        svg.push_str("</svg>\n");
        svg
    }

    /// G3.13: Visualize the aggregation graph as an interactive HTML page.
    ///
    /// Wraps the SVG in an HTML document with basic styling and a title.
    pub fn visualize_html(&self) -> String {
        let svg = self.visualize_svg();
        format!(
            "<!DOCTYPE html>\n<html>\n<head>\n  <meta charset=\"utf-8\">\n  \
             <title>Aggregation Graph Visualization</title>\n  \
             <style>\n    body {{ font-family: sans-serif; margin: 20px; }}\n    \
             h1 {{ color: #333; }}\n  </style>\n</head>\n<body>\n  \
             <h1>Aggregation Graph</h1>\n  {}\n</body>\n</html>\n",
            svg
        )
    }
}

impl Default for AggregationGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_graph_add_and_query_edges() {
        let graph = DependencyGraph::new();
        let a = TaskId::compute("a", b"1");
        let b = TaskId::compute("b", b"2");
        let c = TaskId::compute("c", b"3");

        // a depends on b, b depends on c
        graph.add_edge(a, b);
        graph.add_edge(b, c);

        assert_eq!(graph.dependencies(&a), vec![b].into_iter().collect());
        assert_eq!(graph.dependencies(&b), vec![c].into_iter().collect());
        assert_eq!(graph.dependents(&c), vec![b].into_iter().collect());
        assert_eq!(graph.dependents(&b), vec![a].into_iter().collect());
    }

    #[test]
    fn dependency_graph_mark_dirty_propagates() {
        let graph = DependencyGraph::new();
        let a = TaskId::compute("a", b"1");
        let b = TaskId::compute("b", b"2");
        let c = TaskId::compute("c", b"3");

        // a → b → c (a depends on b, b depends on c)
        graph.add_edge(a, b);
        graph.add_edge(b, c);

        // Mark c dirty — should propagate to b and a
        let dirty = graph.mark_dirty(c);

        assert!(dirty.contains(&c));
        assert!(dirty.contains(&b));
        assert!(dirty.contains(&a));
        assert_eq!(graph.status(&c), TaskStatus::Dirty);
        assert_eq!(graph.status(&b), TaskStatus::Dirty);
        assert_eq!(graph.status(&a), TaskStatus::Dirty);
    }

    #[test]
    fn dependency_graph_roots_and_leaves() {
        let graph = DependencyGraph::new();
        let a = TaskId::compute("a", b"1");
        let b = TaskId::compute("b", b"2");
        let c = TaskId::compute("c", b"3");

        graph.add_edge(a, b);
        graph.add_edge(b, c);

        let roots = graph.roots();
        let leaves = graph.leaves();

        assert!(roots.contains(&a));
        assert!(leaves.contains(&c));
    }

    #[test]
    fn dependency_graph_remove_task() {
        let graph = DependencyGraph::new();
        let a = TaskId::compute("a", b"1");
        let b = TaskId::compute("b", b"2");

        graph.add_edge(a, b);
        assert!(!graph.is_empty());

        graph.remove(&b);
        assert!(graph.dependencies(&a).is_empty());
    }

    #[test]
    fn aggregation_graph_build_and_query() {
        let dep_graph = DependencyGraph::new();
        let agg_graph = AggregationGraph::new();

        let a = TaskId::compute("a", b"1");
        let b = TaskId::compute("b", b"2");
        let c = TaskId::compute("c", b"3");

        // a → b → c
        dep_graph.add_edge(a, b);
        dep_graph.add_edge(b, c);

        agg_graph.build_from(&dep_graph);

        // a's subtree should have 3 tasks total
        assert_eq!(agg_graph.subtree_total_count(&a), 3);
        assert!(agg_graph.is_subtree_clean(&a));
    }

    #[test]
    fn aggregation_graph_mark_dirty_updates_counts() {
        let dep_graph = DependencyGraph::new();
        let agg_graph = AggregationGraph::new();

        let a = TaskId::compute("a", b"1");
        let b = TaskId::compute("b", b"2");
        let c = TaskId::compute("c", b"3");

        dep_graph.add_edge(a, b);
        dep_graph.add_edge(b, c);
        agg_graph.build_from(&dep_graph);

        // Mark c dirty
        dep_graph.set_status(c, TaskStatus::Dirty);
        agg_graph.mark_dirty(c, &dep_graph);

        // a's subtree should now have dirty tasks
        assert!(agg_graph.subtree_dirty_count(&a) > 0);
        assert!(!agg_graph.is_subtree_clean(&a));
    }

    #[test]
    fn aggregation_node_metrics_track_compute_time() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("metrics_a", b"");
        let b = TaskId::compute("metrics_b", b"");

        dep_graph.add_edge(a, b);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        // Record compute time
        agg_graph.record_compute_time(&b, 50);
        agg_graph.record_compute_time(&a, 100);

        assert_eq!(agg_graph.subtree_compute_ms(&b), 50);
        assert_eq!(agg_graph.subtree_compute_ms(&a), 100);

        // Mark cached
        agg_graph.mark_cached(&b);
        agg_graph.mark_cached(&a);
        assert_eq!(agg_graph.subtree_cached_count(&b), 1);
        assert_eq!(agg_graph.subtree_cached_count(&a), 1);

        // Get metrics summary
        let metrics = agg_graph.get_metrics(&a).expect("node should exist");
        assert!(
            metrics.contains("compute_ms=100"),
            "metrics should contain compute time: {}",
            metrics
        );
        assert!(
            metrics.contains("cached=1"),
            "metrics should contain cached count: {}",
            metrics
        );
    }

    #[test]
    fn incremental_rebuild_only_updates_affected_nodes() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("incr_a", b"");
        let b = TaskId::compute("incr_b", b"");
        let c = TaskId::compute("incr_c", b"");
        let d = TaskId::compute("incr_d", b"");
        let e = TaskId::compute("incr_e", b"");

        // Build graph: a -> b -> c, d -> e (two independent chains)
        dep_graph.add_edge(a, b);
        dep_graph.add_edge(b, c);
        dep_graph.add_edge(d, e);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);
        dep_graph.set_status(c, TaskStatus::Clean);
        dep_graph.set_status(d, TaskStatus::Clean);
        dep_graph.set_status(e, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        // Verify initial state
        assert_eq!(
            agg_graph.subtree_total_count(&a),
            3,
            "a subtree should have 3 tasks"
        );
        assert_eq!(
            agg_graph.subtree_total_count(&d),
            2,
            "d subtree should have 2 tasks"
        );
        assert_eq!(
            agg_graph.subtree_dirty_count(&d),
            0,
            "d should be clean initially"
        );

        // Mark c as dirty — only c, b, a should be affected
        dep_graph.set_status(c, TaskStatus::Dirty);
        agg_graph.incremental_rebuild(&[c], &dep_graph);

        // a and b should now have dirty_count > 0
        assert!(
            agg_graph.subtree_dirty_count(&a) > 0,
            "a should have dirty count after incremental rebuild"
        );
        assert!(
            agg_graph.subtree_dirty_count(&b) > 0,
            "b should have dirty count after incremental rebuild"
        );
        assert!(
            agg_graph.subtree_dirty_count(&c) > 0,
            "c should have dirty count"
        );
        // d should be unaffected
        assert_eq!(
            agg_graph.subtree_dirty_count(&d),
            0,
            "d should still be clean (unaffected)"
        );
    }

    #[test]
    fn incremental_rebuild_adds_new_tasks() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("new_a", b"");
        let b = TaskId::compute("new_b", b"");

        dep_graph.add_edge(a, b);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        // Add a new task c that depends on a (linear chain: c -> a -> b)
        let c = TaskId::compute("new_c", b"");
        dep_graph.add_edge(c, a);
        dep_graph.set_status(c, TaskStatus::Clean);

        // Incremental rebuild — c and its dependents (none in this case)
        agg_graph.incremental_rebuild(&[c], &dep_graph);

        // c should now exist in the aggregation graph
        assert!(
            agg_graph.get(&c).is_some(),
            "c should be added by incremental rebuild"
        );
        assert_eq!(
            agg_graph.subtree_total_count(&c),
            3,
            "c subtree should have 3 tasks (c + a + b)"
        );
        // a should be unaffected (still 2)
        assert_eq!(
            agg_graph.subtree_total_count(&a),
            2,
            "a subtree should still have 2 tasks"
        );
    }

    #[test]
    fn aggregation_graph_visualize_svg_outputs_valid_svg() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("viz_agg_a", b"");
        let b = TaskId::compute("viz_agg_b", b"");

        dep_graph.add_edge(a, b);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        let svg = agg_graph.visualize_svg();
        assert!(svg.starts_with("<svg"), "SVG should start with <svg tag");
        assert!(svg.contains("</svg>"), "SVG should end with </svg>");
        assert!(svg.contains("<rect"), "SVG should contain rectangle nodes");
        assert!(svg.contains("<text"), "SVG should contain text labels");
        assert!(svg.contains("<line"), "SVG should contain edges");
        assert!(
            svg.contains("total="),
            "SVG should contain total count metric"
        );
        assert!(
            svg.contains("dirty="),
            "SVG should contain dirty count metric"
        );
    }

    #[test]
    fn aggregation_graph_visualize_html_outputs_valid_html() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("html_viz_a", b"");
        let b = TaskId::compute("html_viz_b", b"");

        dep_graph.add_edge(a, b);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        let html = agg_graph.visualize_html();
        assert!(
            html.starts_with("<!DOCTYPE html>"),
            "HTML should start with DOCTYPE"
        );
        assert!(html.contains("<svg"), "HTML should contain SVG");
        assert!(html.contains("</html>"), "HTML should end with </html>");
        assert!(
            html.contains("Aggregation Graph"),
            "HTML should contain title"
        );
    }

    #[test]
    fn lazy_aggregation_only_creates_queried_nodes() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("lazy_a", b"");
        let b = TaskId::compute("lazy_b", b"");
        let c = TaskId::compute("lazy_c", b"");
        let d = TaskId::compute("lazy_d", b"");

        // Graph: a -> b -> c, d is independent (no edges with others)
        dep_graph.add_edge(a, b);
        dep_graph.add_edge(b, c);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);
        dep_graph.set_status(c, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();

        // Before any query — no nodes should exist
        assert_eq!(agg_graph.queried_count(), 0, "No queries yet");

        // Query subtree for a — should create nodes for a, b, c only
        let node_a = agg_graph.query_subtree(a, &dep_graph);
        assert!(node_a.is_some(), "Should return node for a");
        assert_eq!(agg_graph.queried_count(), 1, "Should have 1 queried task");
        assert!(agg_graph.is_queried(&a), "a should be marked queried");
        assert!(!agg_graph.is_queried(&d), "d should not be queried");

        // a's subtree should have 3 tasks (a + b + c)
        assert_eq!(
            agg_graph.subtree_total_count(&a),
            3,
            "a subtree should have 3 tasks"
        );

        // d should not have an aggregation node
        assert!(
            agg_graph.get(&d).is_none(),
            "d should not have a node (not queried)"
        );

        // Query d — now it should have a node
        let node_d = agg_graph.query_subtree(d, &dep_graph);
        assert!(node_d.is_some(), "Should return node for d after query");
        assert_eq!(agg_graph.queried_count(), 2, "Should have 2 queried tasks");
        assert_eq!(
            agg_graph.subtree_total_count(&d),
            1,
            "d subtree should have 1 task"
        );
    }

    #[test]
    fn lazy_aggregation_reuses_existing_nodes() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("reuse_a", b"");
        let b = TaskId::compute("reuse_b", b"");

        dep_graph.add_edge(a, b);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Dirty);

        let agg_graph = AggregationGraph::new();

        // First query
        let node1 = agg_graph.query_subtree(a, &dep_graph).unwrap();
        assert_eq!(node1.total_count, 2, "a subtree should have 2 tasks");
        assert_eq!(node1.dirty_count, 1, "a subtree should have 1 dirty task");

        // Second query for same task — should reuse, not create new
        let node2 = agg_graph.query_subtree(a, &dep_graph).unwrap();
        assert_eq!(node2.total_count, 2, "Reused node should have same count");
        assert_eq!(
            agg_graph.queried_count(),
            1,
            "Should still have 1 queried task"
        );
    }

    #[test]
    fn compact_clean_subtree_removes_children() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("comp_a", b"");
        let b = TaskId::compute("comp_b", b"");
        let c = TaskId::compute("comp_c", b"");

        // a -> b -> c, all clean
        dep_graph.add_edge(a, b);
        dep_graph.add_edge(b, c);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);
        dep_graph.set_status(c, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        // Before compaction: 3 nodes
        assert!(
            agg_graph.get(&a).is_some(),
            "a should exist before compaction"
        );
        assert!(
            agg_graph.get(&b).is_some(),
            "b should exist before compaction"
        );
        assert!(
            agg_graph.get(&c).is_some(),
            "c should exist before compaction"
        );
        assert_eq!(
            agg_graph.subtree_total_count(&a),
            3,
            "a subtree should have 3 tasks"
        );

        // Compact a's subtree
        let removed = agg_graph.compact(a, &dep_graph);
        assert_eq!(removed, 2, "Should remove 2 child nodes (b and c)");

        // After compaction: only a remains, b and c removed
        assert!(
            agg_graph.get(&a).is_some(),
            "a should still exist after compaction"
        );
        assert!(
            agg_graph.get(&b).is_none(),
            "b should be removed after compaction"
        );
        assert!(
            agg_graph.get(&c).is_none(),
            "c should be removed after compaction"
        );

        // a should still have correct aggregated counts
        let node_a = agg_graph.get(&a).unwrap();
        assert_eq!(
            node_a.total_count, 3,
            "Compacted node should preserve total_count"
        );
        assert_eq!(
            node_a.dirty_count, 0,
            "Compacted node should preserve dirty_count"
        );
        assert!(node_a.compacted, "Node should be marked compacted");
        assert!(
            node_a.children.is_empty(),
            "Compacted node should have no children"
        );
    }

    #[test]
    fn compact_does_not_compact_dirty_subtree() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("dirty_comp_a", b"");
        let b = TaskId::compute("dirty_comp_b", b"");

        dep_graph.add_edge(a, b);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Dirty);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        // Subtree is dirty — compaction should do nothing
        let removed = agg_graph.compact(a, &dep_graph);
        assert_eq!(removed, 0, "Should not compact dirty subtree");
        assert!(
            agg_graph.get(&b).is_some(),
            "b should still exist (not compacted)"
        );
    }

    #[test]
    fn compact_all_compacts_all_clean_roots() {
        let dep_graph = DependencyGraph::new();
        let a = TaskId::compute("all_comp_a", b"");
        let b = TaskId::compute("all_comp_b", b"");
        let c = TaskId::compute("all_comp_c", b"");
        let d = TaskId::compute("all_comp_d", b"");

        // a -> b (clean), c -> d (clean), two independent roots
        dep_graph.add_edge(a, b);
        dep_graph.add_edge(c, d);
        dep_graph.set_status(a, TaskStatus::Clean);
        dep_graph.set_status(b, TaskStatus::Clean);
        dep_graph.set_status(c, TaskStatus::Clean);
        dep_graph.set_status(d, TaskStatus::Clean);

        let agg_graph = AggregationGraph::new();
        agg_graph.build_from(&dep_graph);

        let removed = agg_graph.compact_all(&dep_graph);
        assert_eq!(removed, 2, "Should remove 2 child nodes total (b and d)");
        assert_eq!(
            agg_graph.compacted_count(),
            2,
            "Should have 2 compacted root nodes"
        );
    }
}
