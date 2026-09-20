// Zig-backed task dependency graph.
//
// `ZigTaskGraph` wraps the Zig arena-allocated `TaskGraph`
// (native-sys/zig/graph.zig) and implements `TaskGraphOps`, making it a
// drop-in backend for `TaskEngine`'s dependency graph. The Zig arena
// provides:
//
//   • 24-byte nodes (vs ~48+B + HashSet overhead per DashMap entry)
//   • Flat u32 edge arrays — traversal is sequential memory access
//   • O(1) bump-pointer allocation; one arena free tears down everything
//   • Single-FFI-call dirty propagation (`pledge_task_graph_mark_dirty`
//     does BFS + status writes inside Zig, no per-node round trips)
//   • Flat status scans (`ids_by_status`) — one pass over contiguous nodes
//
// The Zig side has no internal locking, so this wrapper serializes all
// access through a `Mutex`. Reads that would benefit from lock-free access
// still take the lock — correctness over speculation; the lock is
// uncontended in the common single-build-thread case and cheap when
// contended because critical sections are nanoseconds.

use crate::graph::{TaskGraphOps, TaskStatus};
use crate::task::TaskId;
use pledgepack_native_sys::TaskGraph as ZigHandle;
use std::collections::HashSet;
use std::sync::Mutex;

/// Zig TaskStatus values (must match `TaskStatus` in native-sys/zig/graph.zig).
mod status {
    pub const CLEAN: u8 = 0;
    pub const DIRTY: u8 = 1;
    pub const COMPUTING: u8 = 2;
    pub const ERROR: u8 = 3;
    pub const PENDING: u8 = 4;
    // 5 = evicted (no Rust counterpart; mapped to Pending on read)
}

fn to_zig(status: TaskStatus) -> u8 {
    match status {
        TaskStatus::Clean => status::CLEAN,
        TaskStatus::Dirty => status::DIRTY,
        TaskStatus::Computing => status::COMPUTING,
        TaskStatus::Error => status::ERROR,
        TaskStatus::Pending => status::PENDING,
    }
}

fn from_zig(raw: u8) -> TaskStatus {
    match raw {
        status::CLEAN => TaskStatus::Clean,
        status::DIRTY => TaskStatus::Dirty,
        status::COMPUTING => TaskStatus::Computing,
        status::ERROR => TaskStatus::Error,
        _ => TaskStatus::Pending,
    }
}

/// A Zig arena-backed task dependency graph — the default `TaskGraphOps`
/// backend when the `zig-graph` feature is enabled.
pub struct ZigTaskGraph {
    inner: Mutex<ZigHandle>,
}

impl ZigTaskGraph {
    /// Create an empty Zig-backed task graph.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ZigHandle::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ZigHandle> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for ZigTaskGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskGraphOps for ZigTaskGraph {
    fn add_edge(&self, parent: TaskId, child: TaskId) {
        self.lock().add_edge(parent.as_bytes(), child.as_bytes());
    }

    fn add_edges(&self, parent: TaskId, children: &[TaskId]) {
        let g = self.lock();
        for child in children {
            g.add_edge(parent.as_bytes(), child.as_bytes());
        }
    }

    fn dependents(&self, task: &TaskId) -> HashSet<TaskId> {
        self.lock()
            .dependents(task.as_bytes())
            .into_iter()
            .map(TaskId::from_bytes)
            .collect()
    }

    fn dependencies(&self, task: &TaskId) -> HashSet<TaskId> {
        self.lock()
            .dependencies(task.as_bytes())
            .into_iter()
            .map(TaskId::from_bytes)
            .collect()
    }

    fn status(&self, task: &TaskId) -> TaskStatus {
        from_zig(self.lock().status(task.as_bytes()))
    }

    fn set_status(&self, task: TaskId, status: TaskStatus) {
        let g = self.lock();
        g.add_task(task.as_bytes());
        g.set_status(task.as_bytes(), to_zig(status));
    }

    fn mark_dirty(&self, task: TaskId) -> HashSet<TaskId> {
        let g = self.lock();
        // The Rust backend marks a task dirty even when it has no graph
        // node yet (e.g. invalidated before first compute) — materialize
        // the node first so dirty_tasks() reports it identically.
        g.add_task(task.as_bytes());
        g.mark_dirty(task.as_bytes())
            .into_iter()
            .map(TaskId::from_bytes)
            .collect()
    }

    fn mark_clean(&self, task: TaskId) {
        self.set_status(task, TaskStatus::Clean);
    }

    fn dirty_tasks(&self) -> Vec<TaskId> {
        self.lock()
            .ids_by_status(status::DIRTY)
            .into_iter()
            .map(TaskId::from_bytes)
            .collect()
    }

    fn clean_tasks(&self) -> Vec<TaskId> {
        self.lock()
            .ids_by_status(status::CLEAN)
            .into_iter()
            .map(TaskId::from_bytes)
            .collect()
    }

    fn all_tasks(&self) -> Vec<TaskId> {
        self.lock()
            .all_ids()
            .into_iter()
            .map(TaskId::from_bytes)
            .collect()
    }

    fn clear(&self) {
        self.lock().clear();
    }

    fn len(&self) -> usize {
        self.lock().task_count()
    }
}

impl std::fmt::Debug for ZigTaskGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZigTaskGraph")
            .field("task_count", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zig_graph_add_and_query_edges() {
        let graph = ZigTaskGraph::new();
        let a = TaskId::compute("test", b"a");
        let b = TaskId::compute("test", b"b");
        let c = TaskId::compute("test", b"c");

        // a → b → c (a depends on b, b depends on c)
        graph.add_edge(a, b);
        graph.add_edge(b, c);

        assert_eq!(graph.len(), 3);
        assert!(graph.dependencies(&a).contains(&b));
        assert!(graph.dependencies(&b).contains(&c));
        assert!(graph.dependents(&c).contains(&b));
        assert!(graph.dependents(&b).contains(&a));
    }

    #[test]
    fn zig_graph_mark_dirty_propagates() {
        let graph = ZigTaskGraph::new();
        let a = TaskId::compute("test", b"a");
        let b = TaskId::compute("test", b"b");
        let c = TaskId::compute("test", b"c");

        graph.add_edge(a, b);
        graph.add_edge(b, c);
        for t in [a, b, c] {
            graph.set_status(t, TaskStatus::Clean);
        }

        // Mark c dirty — one FFI call must dirty c, b, and a.
        let dirtied = graph.mark_dirty(c);
        assert_eq!(dirtied.len(), 3);
        for t in [a, b, c] {
            assert_eq!(graph.status(&t), TaskStatus::Dirty);
        }
        assert_eq!(graph.dirty_tasks().len(), 3);
    }

    #[test]
    fn zig_graph_status_scans_and_clear() {
        let graph = ZigTaskGraph::new();
        let a = TaskId::compute("test", b"a");
        let b = TaskId::compute("test", b"b");
        graph.add_edge(a, b);
        graph.set_status(a, TaskStatus::Clean);
        graph.set_status(b, TaskStatus::Dirty);

        assert_eq!(graph.clean_tasks(), vec![a]);
        assert_eq!(graph.dirty_tasks(), vec![b]);
        assert_eq!(graph.all_tasks().len(), 2);

        graph.clear();
        assert_eq!(graph.len(), 0);
        assert!(graph.all_tasks().is_empty());
    }

    /// Parity check: identical op sequences on the Rust DashMap backend and
    /// the Zig backend must yield identical observable state.
    #[test]
    fn zig_graph_parity_with_rust_backend() {
        let rust = crate::graph::DependencyGraph::new();
        let zig = ZigTaskGraph::new();

        let ids: Vec<TaskId> = (0..64u32)
            .map(|i| TaskId::compute("parity", &i.to_le_bytes()))
            .collect();

        // Chain + fan-in edges: i depends on i-1; every 8th also on i-2, i-3.
        for i in 1..ids.len() {
            rust.add_edge(ids[i], ids[i - 1]);
            zig.add_edge(ids[i], ids[i - 1]);
            if i % 8 == 0 {
                rust.add_edges(ids[i], &[ids[i - 2], ids[i - 3]]);
                zig.add_edges(ids[i], &[ids[i - 2], ids[i - 3]]);
            }
        }
        for &id in &ids {
            rust.set_status(id, TaskStatus::Clean);
            zig.set_status(id, TaskStatus::Clean);
        }

        assert_eq!(rust.len(), zig.len());
        for &id in &ids {
            assert_eq!(rust.dependencies(&id), zig.dependencies(&id));
            assert_eq!(rust.dependents(&id), zig.dependents(&id));
        }

        let rust_dirty = rust.mark_dirty(ids[20]);
        let zig_dirty = zig.mark_dirty(ids[20]);
        assert_eq!(rust_dirty, zig_dirty);

        let mut rust_list = rust.dirty_tasks();
        let mut zig_list = zig.dirty_tasks();
        rust_list.sort();
        zig_list.sort();
        assert_eq!(rust_list, zig_list);
    }
}
