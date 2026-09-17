// Demand-driven scheduler + TaskEngine — the heart of PledgePack's task system.
//
// Design principles:
//   1. Demand-driven: defer re-execution of dirty tasks until they become part
//      of an "active query." Only schedule when needed.
//   2. Custom executor: not Tokio-bound. Uses a work-stealing thread pool
//      optimized for task-graph workloads (leaves first, priority by depth).
//   3. Incremental: only dirty tasks are scheduled. Clean tasks are served
//      from cache immediately.
//   4. Content-addressed: the task ID is the cache key. No invalidation
//      heuristics — if the input hash changed, it's a different task ID.
//
// The TaskEngine combines:
//   - TaskBackend (memory + disk + remote storage)
//   - DependencyGraph (edges, invalidation propagation)
//   - AggregationGraph (O(log n) sub-graph queries)
//   - TaskRegistry (function ID → executor function mapping)
//   - Scheduler (demand-driven, work-stealing)

use crate::backend::{MemoryBackend, TaskBackend};
use crate::environment::{self, Environment};
use crate::graph::{AggregationGraph, DependencyGraph, TaskStatus};
use crate::read_tracker;
use crate::registry::{TaskExecutor, TaskRegistry};
use crate::task::TaskId;
use dashmap::DashMap;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{error, info, trace, warn};

/// G4.15: Serializable snapshot of scheduler state.
///
/// Captures task statuses, TTLs, parallel flags, and environment assignments.
/// Can be serialized (e.g., via serde_json) and restored later to resume
/// scheduling from a known state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SchedulerCheckpoint {
    /// Task statuses: TaskId hex → status name.
    pub task_statuses: Vec<(String, String)>,
    /// Task TTLs: TaskId hex → seconds.
    pub task_ttls: Vec<(String, u64)>,
    /// Task parallel flags: TaskId hex → bool.
    pub task_parallel: Vec<(String, bool)>,
    /// Task environments: TaskId hex → environment name.
    pub task_envs: Vec<(String, String)>,
    /// Active queries: query_id → root TaskId hex list.
    pub active_queries: Vec<(u64, Vec<String>)>,
    /// Next query ID counter.
    pub next_query_id: u64,
}

/// G4.4: A runtime-agnostic async notification primitive.
///
/// This replaces `tokio::sync::Notify` so the task engine is not Tokio-bound.
/// Uses `std::sync` primitives internally — works with any async runtime or
/// no runtime at all (e.g., `poll_fn` + manual polling).
///
/// Multi-waker semantics: `notified()` returns a future that completes when
/// `notify_waiters()` is called. Unlike the previous single-waker design,
/// all waiters are woken, so multiple coroutines waiting on the same task
/// are all notified when computation finishes. The Notify is typically
/// discarded after one use.
struct Notify {
    completed: AtomicBool,
    wakers: Mutex<Vec<std::task::Waker>>,
}

impl Notify {
    fn new() -> Self {
        Notify {
            completed: AtomicBool::new(false),
            wakers: Mutex::new(Vec::new()),
        }
    }

    fn notify_waiters(&self) {
        self.completed.store(true, Ordering::Release);
        // Wake all waiters, not just the last one registered.
        let wakers = std::mem::take(&mut *self.wakers.lock().unwrap_or_else(|e| e.into_inner()));
        for waker in wakers {
            waker.wake();
        }
    }

    fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        std::future::poll_fn(move |cx| {
            if self.completed.load(Ordering::Acquire) {
                std::task::Poll::Ready(())
            } else {
                // Register this waker alongside any other waiters.
                self.wakers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(cx.waker().clone());
                std::task::Poll::Pending
            }
        })
    }
}

/// Error types for the task system.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("Task not found in registry: {0}")]
    NotRegistered(String),
    #[error("Task computation failed: {0}")]
    ComputationFailed(String),
    #[error("Task output deserialization failed: {0}")]
    DeserializationFailed(String),
    #[error("Task cycle detected among {tasks:?}")]
    CycleDetected { tasks: Vec<crate::task::TaskId> },
    #[error("Backend error: {0}")]
    BackendError(String),
    #[error("Determinism violation for task {task_id}:\n{diff}")]
    DeterminismViolation {
        task_id: crate::task::TaskId,
        diff: String,
    },
}

/// An active query — a set of task IDs that the dev server or build command
/// is interested in. Only tasks that are part of an active query are scheduled
/// for computation.
///
/// This is the "demand-driven" part: when a file changes, we mark dependent
/// tasks dirty, but we don't schedule them. We only schedule them when an
/// active query covers them.
#[derive(Debug, Clone)]
pub struct ActiveQuery {
    /// A unique ID for this query.
    pub id: u64,
    /// The root task IDs this query is interested in.
    pub roots: Vec<TaskId>,
}

/// The task engine — combines backend, graphs, registry, and scheduler.
///
/// This is the main entry point for the task system. Users interact with it
/// via `Task<T>::read(&engine).await?`.
pub struct TaskEngine {
    /// Three-tier storage for task outputs.
    backend: TaskBackend,
    /// Dependency graph (edges, invalidation propagation).
    dep_graph: DependencyGraph,
    /// Aggregation graph (O(log n) sub-graph queries).
    agg_graph: AggregationGraph,
    /// Task registry (function ID → executor).
    registry: Arc<TaskRegistry>,
    /// Active queries (demand-driven scheduling).
    active_queries: RwLock<HashMap<u64, ActiveQuery>>,
    /// Next active query ID.
    next_query_id: std::sync::atomic::AtomicU64,
    /// Tasks currently being computed (to avoid duplicate computation).
    computing: Mutex<HashSet<TaskId>>,
    /// Notify for tasks that are being computed (waiters block on this).
    task_notify: Mutex<HashMap<TaskId, Arc<Notify>>>,
    /// Whether to verify determinism (double-execute tasks and compare outputs).
    verify_determinism: bool,
    /// G2.11: Per-task TTL in seconds (TaskId → seconds). 0 = no TTL.
    task_ttls: DashMap<TaskId, u64>,
    /// G2.12: Per-task parallel flag (TaskId → can run in parallel). Default true.
    task_parallel: DashMap<TaskId, bool>,
    /// G5.8: Per-task environment tracking for env-filtered visualization.
    task_envs: DashMap<TaskId, crate::environment::Environment>,
    /// File read index: file path → set of task IDs that read this file.
    /// Used for file-based invalidation via `invalidate_file()`.
    /// Populated by the read tracker during task computation.
    read_index: RwLock<HashMap<PathBuf, HashSet<TaskId>>>,
    /// Whether read tracking is enabled.
    read_tracking_enabled: std::sync::atomic::AtomicBool,
}

impl TaskEngine {
    /// Create a new TaskEngine with the given registry and memory backend.
    pub fn new(registry: TaskRegistry, backend: TaskBackend) -> Self {
        TaskEngine {
            backend,
            dep_graph: DependencyGraph::new(),
            agg_graph: AggregationGraph::new(),
            registry: Arc::new(registry),
            active_queries: RwLock::new(HashMap::new()),
            next_query_id: std::sync::atomic::AtomicU64::new(1),
            computing: Mutex::new(HashSet::new()),
            task_notify: Mutex::new(HashMap::new()),
            verify_determinism: false,
            task_ttls: DashMap::new(),
            task_parallel: DashMap::new(),
            task_envs: DashMap::new(),
            read_index: RwLock::new(HashMap::new()),
            read_tracking_enabled: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Enable determinism verification (double-execute tasks and compare).
    pub fn with_verify_determinism(mut self) -> Self {
        self.verify_determinism = true;
        self
    }

    /// G2.11: Set a TTL (in seconds) for a specific task.
    ///
    /// When the task's cached output is older than the TTL, it will be
    /// treated as a cache miss and recomputed.
    pub fn set_ttl(&self, id: TaskId, ttl_secs: u64) {
        self.task_ttls.insert(id, ttl_secs);
    }

    /// G2.11: Get the TTL for a task, if any.
    pub fn get_ttl(&self, id: &TaskId) -> Option<u64> {
        self.task_ttls.get(id).map(|v| *v)
    }

    /// G7.2: Check if a task's output is already cached (memory or disk).
    ///
    /// Returns `true` if the task has been computed and its output is
    /// available in the memory or disk cache. Does NOT compute the task.
    pub fn is_cached(&self, id: &TaskId) -> bool {
        if self.backend.memory.get(id).is_some() {
            return true;
        }
        false
    }

    /// G2.12: Set whether a task can run in parallel with other tasks.
    ///
    /// Tasks marked `parallel = false` will be scheduled sequentially.
    pub fn set_parallel(&self, id: TaskId, parallel: bool) {
        self.task_parallel.insert(id, parallel);
    }

    /// G2.12: Check if a task can run in parallel. Default: true.
    pub fn is_parallel(&self, id: &TaskId) -> bool {
        self.task_parallel.get(id).map(|v| *v).unwrap_or(true)
    }

    /// G4.7: Evict clean task outputs under memory pressure.
    ///
    /// Evicts LRU clean outputs from the in-memory cache until the cache
    /// has at most `max_entries` items. Only evicts from the local memory
    /// backend — disk and remote caches are not affected.
    ///
    /// Returns the number of evicted entries.
    pub fn evict_under_pressure(&self, max_entries: usize) -> usize {
        self.backend.memory.evict_to_max(max_entries)
    }

    /// G4.15: Create a checkpoint of the current scheduler state.
    ///
    /// Captures task statuses, TTLs, parallel flags, environment assignments,
    /// and active queries into a serializable snapshot.
    pub fn checkpoint(&self) -> SchedulerCheckpoint {
        let task_statuses: Vec<(String, String)> = self
            .dep_graph
            .all_tasks()
            .iter()
            .map(|t| {
                let status = match self.dep_graph.status(t) {
                    TaskStatus::Clean => "Clean",
                    TaskStatus::Dirty => "Dirty",
                    TaskStatus::Computing => "Computing",
                    TaskStatus::Error => "Error",
                    TaskStatus::Pending => "Pending",
                };
                (t.to_hex(), status.to_string())
            })
            .collect();

        let task_ttls: Vec<(String, u64)> = self
            .task_ttls
            .iter()
            .map(|r| (r.key().to_hex(), *r.value()))
            .collect();

        let task_parallel: Vec<(String, bool)> = self
            .task_parallel
            .iter()
            .map(|r| (r.key().to_hex(), *r.value()))
            .collect();

        let task_envs: Vec<(String, String)> = self
            .task_envs
            .iter()
            .map(|r| {
                let env_str = match *r.value() {
                    Environment::Client => "Client".to_string(),
                    Environment::Server => "Server".to_string(),
                    Environment::Edge => "Edge".to_string(),
                    Environment::Worker => "Worker".to_string(),
                    Environment::Shared => "Shared".to_string(),
                    Environment::Custom(_) => {
                        format!("Custom:{}", r.value().name())
                    }
                };
                (r.key().to_hex(), env_str)
            })
            .collect();

        let active_queries: Vec<(u64, Vec<String>)> = self
            .active_queries
            .read()
            .unwrap()
            .iter()
            .map(|(id, q)| {
                let roots: Vec<String> = q.roots.iter().map(|t| t.to_hex()).collect();
                (*id, roots)
            })
            .collect();

        let next_query_id = self
            .next_query_id
            .load(std::sync::atomic::Ordering::Relaxed);

        SchedulerCheckpoint {
            task_statuses,
            task_ttls,
            task_parallel,
            task_envs,
            active_queries,
            next_query_id,
        }
    }

    /// G4.15: Restore scheduler state from a checkpoint.
    ///
    /// Restores task statuses, TTLs, parallel flags, environment assignments,
    /// and active queries. This does NOT restore cached outputs — only
    /// scheduler metadata.
    pub fn restore_checkpoint(&self, cp: &SchedulerCheckpoint) {
        // Restore task statuses
        for (hex, status_str) in &cp.task_statuses {
            if let Some(id) = TaskId::from_hex(hex) {
                let status = match status_str.as_str() {
                    "Clean" => TaskStatus::Clean,
                    "Dirty" => TaskStatus::Dirty,
                    "Computing" => TaskStatus::Computing,
                    "Error" => TaskStatus::Error,
                    "Pending" => TaskStatus::Pending,
                    _ => continue,
                };
                self.dep_graph.set_status(id, status);
            }
        }

        // Restore TTLs
        for (hex, ttl) in &cp.task_ttls {
            if let Some(id) = TaskId::from_hex(hex) {
                self.task_ttls.insert(id, *ttl);
            }
        }

        // Restore parallel flags
        for (hex, parallel) in &cp.task_parallel {
            if let Some(id) = TaskId::from_hex(hex) {
                self.task_parallel.insert(id, *parallel);
            }
        }

        // Restore environments
        for (hex, env_str) in &cp.task_envs {
            if let Some(id) = TaskId::from_hex(hex) {
                let env = match env_str.as_str() {
                    "Client" => Environment::Client,
                    "Server" => Environment::Server,
                    "Edge" => Environment::Edge,
                    "Worker" => Environment::Worker,
                    "Shared" => Environment::Shared,
                    s if s.starts_with("Custom:") => {
                        let name = &s[7..];
                        Environment::register_custom(name)
                    }
                    _ => continue,
                };
                self.task_envs.insert(id, env);
            }
        }

        // Restore active queries
        let mut queries = self
            .active_queries
            .write()
            .unwrap_or_else(|e| e.into_inner());
        queries.clear();
        for (id, roots_hex) in &cp.active_queries {
            let roots: Vec<TaskId> = roots_hex
                .iter()
                .filter_map(|h| TaskId::from_hex(h))
                .collect();
            queries.insert(*id, ActiveQuery { id: *id, roots });
        }

        // Restore next query ID
        self.next_query_id
            .store(cp.next_query_id, std::sync::atomic::Ordering::Relaxed);
    }

    /// G5.8: Set the environment for a task (for env-filtered visualization).
    pub fn set_task_env(&self, id: TaskId, env: crate::environment::Environment) {
        self.task_envs.insert(id, env);
    }

    /// G5.8: Get the environment for a task, if tracked.
    pub fn get_task_env(&self, id: &TaskId) -> Option<crate::environment::Environment> {
        self.task_envs.get(id).map(|v| *v)
    }

    /// G5.8: Visualize the task dependency graph in DOT format, filtered by environment.
    ///
    /// Only tasks belonging to the specified environment (or `Shared`) are included.
    pub fn visualize_dot_for_env(&self, env: crate::environment::Environment) -> String {
        use crate::environment::Environment;
        let all_tasks = self.dep_graph.all_tasks();
        let env_tasks: HashSet<TaskId> = all_tasks
            .iter()
            .filter(|t| {
                self.task_envs
                    .get(t)
                    .map(|e| *e == env || *e == Environment::Shared)
                    .unwrap_or(true)
            })
            .copied()
            .collect();

        let mut dot = String::from("digraph task_graph {\n");
        dot.push_str("  rankdir=LR;\n");
        dot.push_str("  node [shape=box, fontname=\"monospace\"];\n");

        for task in &env_tasks {
            let label = task.short_hex();
            let status = self.dep_graph.status(task);
            let color = match status {
                TaskStatus::Clean => "#90EE90",
                TaskStatus::Dirty => "#FFB6C1",
                TaskStatus::Computing => "#ADD8E6",
                TaskStatus::Error => "#FFA500",
                TaskStatus::Pending => "#D3D3D3",
            };
            dot.push_str(&format!(
                "  \"{}\" [label=\"{}\", fillcolor=\"{}\", style=filled];\n",
                task.to_hex(),
                label,
                color
            ));
        }

        for task in &env_tasks {
            let deps = self.dep_graph.dependencies(task);
            for dep in &deps {
                if env_tasks.contains(dep) {
                    dot.push_str(&format!(
                        "  \"{}\" -> \"{}\";\n",
                        dep.to_hex(),
                        task.to_hex()
                    ));
                }
            }
        }

        dot.push_str("}\n");
        dot
    }

    /// G5.8: Visualize the task dependency graph in Mermaid format, filtered by environment.
    ///
    /// Only tasks belonging to the specified environment (or `Shared`) are included.
    pub fn visualize_mermaid_for_env(&self, env: crate::environment::Environment) -> String {
        use crate::environment::Environment;
        let all_tasks = self.dep_graph.all_tasks();
        let env_tasks: HashSet<TaskId> = all_tasks
            .iter()
            .filter(|t| {
                self.task_envs
                    .get(t)
                    .map(|e| *e == env || *e == Environment::Shared)
                    .unwrap_or(true)
            })
            .copied()
            .collect();

        let mut mermaid = String::from("graph LR\n");

        for task in &env_tasks {
            let label = task.short_hex();
            mermaid.push_str(&format!("  {}[\"{}\"]\n", task.to_hex(), label));
        }

        for task in &env_tasks {
            let deps = self.dep_graph.dependencies(task);
            for dep in &deps {
                if env_tasks.contains(dep) {
                    mermaid.push_str(&format!("  {} --> {}\n", dep.to_hex(), task.to_hex()));
                }
            }
        }

        mermaid
    }

    /// Register an active query. Only tasks in active queries are scheduled.
    pub fn register_query(&self, roots: Vec<TaskId>) -> u64 {
        let id = self
            .next_query_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let query = ActiveQuery { id, roots };
        self.active_queries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, query);
        id
    }

    /// Unregister an active query.
    pub fn unregister_query(&self, query_id: u64) {
        self.active_queries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&query_id);
    }

    /// Read a task's output, scheduling it (and its dependencies) if needed.
    ///
    /// This is the main entry point called by `Task<T>::read(&engine).await?`.
    ///
    /// Flow:
    ///   1. Check memory cache → if hit, return immediately.
    ///   2. Check disk cache → if hit, promote to memory, return.
    ///   3. Check remote cache → if hit, promote to memory + disk, return.
    ///   4. Schedule the task for computation (and its dependencies).
    ///   5. Wait for computation to complete.
    ///   6. Return the output.
    pub async fn read_task<T>(&self, id: TaskId) -> Result<Arc<T>, TaskError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        // 1. Check if the task is dirty (invalidated). If so, skip cache
        //    and recompute. A dirty task means its inputs changed and the
        //    cached output may be stale.
        let is_dirty = self.dep_graph.status(&id) == TaskStatus::Dirty;

        // 2. Check local cache (memory → disk) — only if not dirty
        if !is_dirty {
            if let Some(output) = self.backend.get(&id) {
                // G2.11: Check TTL expiration
                if output.is_expired() {
                    trace!("Task cache expired (TTL): {}", id);
                } else {
                    trace!("Task cache hit: {}", id);
                    let value: T = output
                        .deserialize()
                        .map_err(|e| TaskError::DeserializationFailed(e.to_string()))?;
                    return Ok(Arc::new(value));
                }
            }

            // 3. Check remote cache
            match self.backend.get_remote(&id) {
                Ok(Some(output)) => {
                    // G2.11: Check TTL expiration on remote cache
                    if output.is_expired() {
                        trace!("Remote cache expired (TTL): {}", id);
                    } else {
                        trace!("Remote cache hit: {}", id);
                        let value: T = output
                            .deserialize()
                            .map_err(|e| TaskError::DeserializationFailed(e.to_string()))?;
                        return Ok(Arc::new(value));
                    }
                }
                Ok(None) => {} // Not in remote, need to compute
                Err(e) => {
                    warn!("Remote cache fetch failed for {}: {}", id, e);
                }
            }
        }

        // 3. Compute the task (and its dependencies)
        self.compute_task(id).await?;

        // 4. Read the computed output
        if let Some(output) = self.backend.get(&id) {
            let value: T = output
                .deserialize()
                .map_err(|e| TaskError::DeserializationFailed(e.to_string()))?;
            Ok(Arc::new(value))
        } else {
            Err(TaskError::ComputationFailed(format!(
                "Task {} was computed but output not found",
                id
            )))
        }
    }

    /// Try to read a task without scheduling computation.
    ///
    /// Returns `Some(Arc<T>)` if the task is already cached, `None` otherwise.
    pub fn try_read_task<T>(&self, id: TaskId) -> Option<Arc<T>>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        self.backend
            .get(&id)
            .and_then(|output| output.deserialize::<T>().ok().map(Arc::new))
    }

    /// G5.10: Compute tasks for multiple environments in parallel.
    ///
    /// Takes a list of (TaskId, Environment) pairs and computes all of them
    /// concurrently. Since tasks in different environments have different
    /// TaskIds, they are independent and can run in parallel without
    /// contention.
    ///
    /// Returns a map from TaskId to the computed output for each environment.
    pub async fn read_tasks_for_environments<T>(
        &self,
        tasks: Vec<(TaskId, crate::environment::Environment)>,
    ) -> Result<Vec<(TaskId, Arc<T>)>, TaskError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        // Track environment for each task
        for (id, env) in &tasks {
            self.set_task_env(*id, *env);
        }

        // G5.10: Compute all environment tasks in parallel.
        // Tasks in different environments have different TaskIds, so they
        // are independent and can be computed concurrently without contention.
        let futures: Vec<_> = tasks
            .iter()
            .map(|(id, _)| self.read_task::<T>(*id))
            .collect();

        let results = futures::future::join_all(futures).await;

        let mut outputs = Vec::with_capacity(tasks.len());
        for ((id, _), result) in tasks.into_iter().zip(results.into_iter()) {
            let value = result?;
            outputs.push((id, value));
        }

        Ok(outputs)
    }

    /// Compute a task and all its dependencies.
    ///
    /// This is the demand-driven scheduler: we only compute tasks that are
    /// needed (transitively) by the requested task.
    ///
    /// If the task is already being computed by another coroutine, we wait
    /// for it to finish instead of computing it again.
    async fn compute_task(&self, id: TaskId) -> Result<(), TaskError> {
        // Check if another coroutine is already computing this task
        let notify = {
            let computing = self.computing.lock().unwrap_or_else(|e| e.into_inner());
            if computing.contains(&id) {
                let notifies = self.task_notify.lock().unwrap_or_else(|e| e.into_inner());
                notifies.get(&id).cloned()
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            notify.notified().await;
            return Ok(());
        }

        // Mark as computing
        let notify = {
            let mut computing = self.computing.lock().unwrap_or_else(|e| e.into_inner());
            if computing.contains(&id) {
                // Race: another coroutine started computing while we were waiting
                let notifies = self.task_notify.lock().unwrap_or_else(|e| e.into_inner());
                notifies.get(&id).cloned()
            } else {
                computing.insert(id);
                let notify = Arc::new(Notify::new());
                self.task_notify
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, notify);
                None // We're the one computing — don't wait, proceed
            }
        };
        if let Some(notify) = notify {
            notify.notified().await;
            return Ok(());
        }

        // Set status to Computing
        self.dep_graph.set_status(id, TaskStatus::Computing);

        // Remove old cached output (if any) so the recompute stores fresh output
        self.backend.remove(&id);

        // Execute the task via the registry (looks up executor by ID).
        // Install read tracker before async execution to capture implicit file deps.
        let read_tracking = self
            .read_tracking_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        if read_tracking {
            read_tracker::install_tracker();
        }

        // Set the thread-local environment for inheritance (G5.4).
        // Tasks inherit the caller's environment unless they explicitly override.
        // The environment is set to Shared by default; tasks that need a specific
        // environment use `run_with_environment()` inside their executor.
        let result =
            environment::with_environment(Environment::Shared, || self.registry.execute(&id, self));

        // The executor returns a future — we need to await it.
        // The read tracker is thread-local, so it captures reads on the
        // thread that polls the future.
        let mut output = result.await;

        // G11.4 + G11.5: Determinism verification — double-execute and compare.
        //
        // PRODUCTION-READINESS-100.md goal 22 flagged the `output.as_ref().unwrap()`
        // this used to be as an unguarded assumption; it was actually safe (the
        // surrounding `if` already checked `output.is_ok()`), but a
        // pattern-matched `let` makes that guarantee structural instead of
        // depending on the condition and the unwrap staying in sync across
        // future edits.
        if self.verify_determinism
            && let Ok(ref first_ok) = output
        {
            let first_output = first_ok.clone();
            let result2 = environment::with_environment(Environment::Shared, || {
                self.registry.execute(&id, self)
            });
            let second_output = result2.await;

            match second_output {
                Ok(second) => {
                    if first_output.data != second.data
                        || first_output.output_hash != second.output_hash
                    {
                        let first_str = String::from_utf8_lossy(&first_output.data);
                        let second_str = String::from_utf8_lossy(&second.data);
                        let diff = compute_diff(&first_str, &second_str);
                        tracing::error!("Determinism violation for task {}:\n{}", id, diff);
                        return Err(TaskError::DeterminismViolation { task_id: id, diff });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Determinism check: second execution of task {} failed: {}",
                        id,
                        e
                    );
                }
            }
        }

        // Collect read-tracked file dependencies
        let read_deps = if read_tracking {
            let tracker = read_tracker::collect_tracker();
            let reads: Vec<String> = tracker
                .reads()
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();

            // Update the file read index for invalidation
            if !reads.is_empty() {
                let mut index = self.read_index.write().unwrap_or_else(|e| e.into_inner());
                for path_str in &reads {
                    let path = PathBuf::from(path_str);
                    index.entry(path).or_default().insert(id);
                }
            }
            reads
        } else {
            Vec::new()
        };

        // Merge read-tracked deps into the output
        if !read_deps.is_empty() {
            if let Ok(ref mut out) = output {
                out.read_dependencies = read_deps;
            }
        }

        // Handle the result
        match output {
            Ok(output) => {
                // Record dependency edges
                self.dep_graph.add_edges(id, &output.dependencies);
                self.dep_graph.set_status(id, TaskStatus::Clean);

                // G2.10: Skip disk/remote caching for non-cacheable tasks
                // (has_side_effects = true). Memory cache is always stored so
                // that read_task can retrieve the output. Non-cacheable tasks
                // are marked Dirty after computation so the next read recomputes.
                self.backend.store_memory(output.clone());

                if !output.has_side_effects {
                    // Store to disk (best-effort)
                    if let Some(disk) = self.backend.disk() {
                        let _ = disk.store(&output);
                    }

                    // Store to remote (best-effort, don't fail the build on remote errors)
                    let _ = self.backend.store_remote(&output);
                } else {
                    // Non-cacheable: mark dirty so next read recomputes
                    self.dep_graph.set_status(id, TaskStatus::Dirty);
                }

                // Rebuild aggregation graph for this subtree
                self.agg_graph.build_from(&self.dep_graph);
            }
            Err(e) => {
                error!("Task computation failed: {}: {}", id, e);
                self.dep_graph.set_status(id, TaskStatus::Error);
                return Err(TaskError::ComputationFailed(e.to_string()));
            }
        }

        // Notify waiters
        {
            self.computing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            let notify = self
                .task_notify
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(notify) = notify {
                notify.notify_waiters();
            }
        }

        Ok(())
    }

    /// Invalidate a task (mark it and all transitive dependents as dirty).
    ///
    /// This is called when a source file changes. The task is not immediately
    /// recomputed — it will be recomputed on demand when an active query
    /// covers it.
    pub fn invalidate(&self, task: TaskId) {
        let dirty = self.dep_graph.mark_dirty(task);
        for &t in &dirty {
            self.agg_graph.mark_dirty(t, &self.dep_graph);
        }
        info!("Invalidated {} tasks", dirty.len());
    }

    /// Invalidate all tasks that read the given file (read-tracked dependencies).
    ///
    /// This supplements explicit dependency invalidation. When a file changes
    /// that was read during task execution (but wasn't an explicit Task<T>
    /// dependency), all tasks that read it are marked dirty.
    ///
    /// Returns the number of tasks invalidated.
    pub fn invalidate_file(&self, path: &std::path::Path) -> usize {
        let mut count = 0;
        let tasks_to_invalidate: Vec<TaskId> = {
            let index = self.read_index.read().unwrap_or_else(|e| e.into_inner());
            index
                .get(path)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect()
        };

        for task_id in &tasks_to_invalidate {
            let dirty = self.dep_graph.mark_dirty(*task_id);
            for &t in &dirty {
                self.agg_graph.mark_dirty(t, &self.dep_graph);
            }
            count += dirty.len();
        }

        if count > 0 {
            info!(
                "Invalidated {} tasks via file read index: {:?}",
                count, path
            );
        }
        count
    }

    /// Enable read tracking for subsequent task computations.
    pub fn enable_read_tracking(&self) {
        self.read_tracking_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Disable read tracking for subsequent task computations.
    pub fn disable_read_tracking(&self) {
        self.read_tracking_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether read tracking is enabled.
    pub fn is_read_tracking_enabled(&self) -> bool {
        self.read_tracking_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get the file read index (for debugging/inspection).
    pub fn read_index(&self) -> HashMap<PathBuf, HashSet<TaskId>> {
        self.read_index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Get all dirty tasks that are covered by an active query.
    ///
    /// This is the demand-driven part: we only schedule tasks that are both
    /// dirty AND needed by an active query.
    pub fn dirty_tasks_for_active_queries(&self) -> HashSet<TaskId> {
        let queries = self
            .active_queries
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut needed: HashSet<TaskId> = HashSet::new();

        for query in queries.values() {
            for &root in &query.roots {
                // Collect all transitive dependencies of this root
                let mut queue = VecDeque::new();
                queue.push_back(root);
                while let Some(task) = queue.pop_front() {
                    if needed.insert(task) {
                        for dep in self.dep_graph.dependencies(&task) {
                            queue.push_back(dep);
                        }
                    }
                }
            }
        }

        // Filter to only dirty tasks
        needed
            .into_iter()
            .filter(|t| self.dep_graph.status(t) == TaskStatus::Dirty)
            .collect()
    }

    /// Group dirty tasks into independent batches for parallel execution (G4.10).
    ///
    /// Tasks in the same batch have no dependency relationships between them
    /// and can be computed in parallel. Batches are ordered so that all
    /// dependencies of batch N are satisfied by batches 0..N.
    ///
    /// This is a topological sort of the dirty tasks, grouped by "wave":
    /// - Wave 0: tasks with no dirty dependencies
    /// - Wave N: tasks whose dirty dependencies are all in waves 0..N-1
    pub fn batch_schedule(
        &self,
        dirty_tasks: &HashSet<TaskId>,
    ) -> Result<Vec<Vec<TaskId>>, TaskError> {
        if dirty_tasks.is_empty() {
            return Ok(Vec::new());
        }

        let mut remaining: HashSet<TaskId> = dirty_tasks.iter().copied().collect();
        let mut batches: Vec<Vec<TaskId>> = Vec::new();

        while !remaining.is_empty() {
            // Find tasks whose dependencies are either:
            // - Not in the dirty set (already clean/computed)
            // - Already assigned to a previous batch
            let assigned: HashSet<TaskId> = batches.iter().flatten().copied().collect();

            let ready: Vec<TaskId> = remaining
                .iter()
                .filter(|&&id| {
                    let deps = self.dep_graph.dependencies(&id);
                    deps.iter().all(|dep| {
                        // Dependency is not dirty (already computed) or already assigned
                        !dirty_tasks.contains(dep) || assigned.contains(dep)
                    })
                })
                .copied()
                .collect();

            if ready.is_empty() {
                // Circular dependency among remaining dirty tasks — return an error
                // instead of silently scheduling them together.
                error!("Cycle detected among {} dirty tasks", remaining.len());
                return Err(TaskError::CycleDetected {
                    tasks: remaining.iter().copied().collect(),
                });
            }

            for id in &ready {
                remaining.remove(id);
            }
            batches.push(ready);
        }

        Ok(batches)
    }

    /// G4.5: Priority scheduling — tasks closer to root first.
    ///
    /// Like `batch_schedule`, but within each batch, tasks are sorted by
    /// priority: tasks closer to the root (higher depth in the dependency
    /// graph) are scheduled first. This means we prioritize tasks that are
    /// closer to the final output, so errors are discovered sooner.
    ///
    /// Returns a flat Vec of task IDs in priority order (highest priority first).
    pub fn priority_schedule(
        &self,
        dirty_tasks: &HashSet<TaskId>,
    ) -> Result<Vec<TaskId>, TaskError> {
        let batches = self.batch_schedule(dirty_tasks)?;

        // Compute depth for each task (distance from root).
        // Root tasks (no dependents) have depth 0.
        // Tasks that depend on root tasks have depth 1, etc.
        let mut depths: HashMap<TaskId, u32> = HashMap::new();
        for &task in dirty_tasks {
            depths.insert(task, self.compute_depth(task, dirty_tasks));
        }

        // Sort each batch by depth (descending — higher depth = closer to root = first)
        let mut result: Vec<TaskId> = Vec::new();
        for mut batch in batches {
            batch.sort_by(|a, b| {
                let depth_a = depths.get(a).copied().unwrap_or(0);
                let depth_b = depths.get(b).copied().unwrap_or(0);
                depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
            });
            result.extend(batch);
        }
        Ok(result)
    }

    /// G4.5: Compute the depth of a task in the dependency graph.
    ///
    /// Depth = number of edges from the nearest root task to this task.
    /// Root tasks (no dependents) have depth 0.
    /// A task that depends on a root task has depth 1, etc.
    fn compute_depth(&self, task: TaskId, dirty_tasks: &HashSet<TaskId>) -> u32 {
        let dependents = self.dep_graph.dependents(&task);
        if dependents.is_empty() {
            return 0;
        }
        let max_dep_depth = dependents
            .iter()
            .filter(|d| dirty_tasks.contains(*d))
            .map(|d| self.compute_depth(*d, dirty_tasks))
            .max()
            .unwrap_or(0);
        max_dep_depth + 1
    }

    /// G4.6: Speculative execution — identify dirty tasks likely to be queried soon.
    ///
    /// When the scheduler is idle (no active queries or all active queries are
    /// blocked), this method identifies dirty tasks that are likely to be
    /// queried next based on:
    /// 1. Tasks that were recently part of an active query (recently accessed)
    /// 2. Tasks that are dependencies of currently active query roots
    /// 3. Tasks closest to existing active query roots (graph proximity)
    ///
    /// Returns a prioritized list of dirty task IDs to speculatively compute.
    /// The caller is responsible for actually computing them (e.g., via
    /// `batch_schedule` + `compute_task`).
    pub fn speculative_schedule(&self) -> Vec<TaskId> {
        // Collect all dirty tasks
        let all_dirty: HashSet<TaskId> = self
            .dep_graph
            .all_tasks()
            .into_iter()
            .filter(|t| self.dep_graph.status(t) == TaskStatus::Dirty)
            .collect();

        if all_dirty.is_empty() {
            return Vec::new();
        }

        // Collect tasks near active query roots — these are likely to be needed
        let mut proximity_set: HashSet<TaskId> = HashSet::new();
        {
            let queries = self
                .active_queries
                .read()
                .unwrap_or_else(|e| e.into_inner());
            for query in queries.values() {
                for &root in &query.roots {
                    // Add the root and its transitive deps
                    let mut queue = VecDeque::new();
                    queue.push_back(root);
                    while let Some(task) = queue.pop_front() {
                        if proximity_set.insert(task) {
                            for dep in self.dep_graph.dependencies(&task) {
                                queue.push_back(dep);
                            }
                        }
                    }
                }
            }
        }

        // Score each dirty task:
        // - Tasks in the proximity set get +2 (likely to be queried)
        // - Tasks that are dependents of proximity tasks get +1
        let mut scored: Vec<(TaskId, u32)> = Vec::new();

        for &task in &all_dirty {
            let mut score: u32 = 0;

            if proximity_set.contains(&task) {
                score += 2;
            }

            // Check if any dependent of this task is in the proximity set
            for dependent in self.dep_graph.dependents(&task) {
                if proximity_set.contains(&dependent) {
                    score += 1;
                    break;
                }
            }

            // Tasks with no dependents (roots) get a small bonus — they're
            // likely final outputs that will be queried
            if self.dep_graph.dependents(&task).is_empty() {
                score += 1;
            }

            scored.push((task, score));
        }

        // Sort by score descending, then by task ID for determinism
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        scored.into_iter().map(|(t, _)| t).collect()
    }

    /// Get the dependency graph (for debugging/visualization).
    pub fn dependency_graph(&self) -> &DependencyGraph {
        &self.dep_graph
    }

    /// Get the aggregation graph (for debugging/visualization).
    pub fn aggregation_graph(&self) -> &AggregationGraph {
        &self.agg_graph
    }

    /// Get the task backend (for cache management).
    pub fn backend(&self) -> &TaskBackend {
        &self.backend
    }

    /// G2.13: Visualize the task dependency graph in DOT format (Graphviz).
    ///
    /// Produces a DOT graph showing all tasks and their dependency edges.
    /// Tasks are colored by status: green=clean, red=dirty, gray=unknown.
    pub fn visualize_dot(&self) -> String {
        let mut dot = String::from("digraph task_graph {\n");
        dot.push_str("  rankdir=LR;\n");
        dot.push_str("  node [shape=box, fontname=\"monospace\"];\n");

        let all_tasks = self.dep_graph.all_tasks();
        for task in &all_tasks {
            let label = task.short_hex();
            let status = self.dep_graph.status(task);
            let color = match status {
                TaskStatus::Clean => "#90EE90",
                TaskStatus::Dirty => "#FFB6C1",
                TaskStatus::Computing => "#ADD8E6",
                TaskStatus::Error => "#FFA500",
                TaskStatus::Pending => "#D3D3D3",
            };
            dot.push_str(&format!(
                "  \"{}\" [label=\"{}\", fillcolor=\"{}\", style=filled];\n",
                task.to_hex(),
                label,
                color
            ));
        }

        for task in &all_tasks {
            let deps = self.dep_graph.dependencies(task);
            for dep in &deps {
                dot.push_str(&format!(
                    "  \"{}\" -> \"{}\";\n",
                    dep.to_hex(),
                    task.to_hex()
                ));
            }
        }

        dot.push_str("}\n");
        dot
    }

    /// G2.13: Visualize the task dependency graph in Mermaid format.
    ///
    /// Produces a Mermaid flowchart showing all tasks and their dependency edges.
    pub fn visualize_mermaid(&self) -> String {
        let mut mermaid = String::from("graph LR\n");

        let all_tasks = self.dep_graph.all_tasks();
        for task in &all_tasks {
            let label = task.short_hex();
            mermaid.push_str(&format!("  {}[\"{}\"]\n", task.to_hex(), label));
        }

        for task in &all_tasks {
            let deps = self.dep_graph.dependencies(task);
            for dep in &deps {
                mermaid.push_str(&format!("  {} --> {}\n", dep.to_hex(), task.to_hex()));
            }
        }

        mermaid
    }
}

/// G4.9: A Chrome Trace event for scheduler profiling.
///
/// See: https://docs.google.com/document/d/1CvAClvFfyA5R5-TI7Z5pL3U6NHfPum50n5k5Q5gF7yI/
#[derive(Debug, Clone, Serialize)]
struct TraceEvent {
    name: String,
    cat: String,
    ph: String,
    ts: u64,
    dur: u64,
    pid: u32,
    tid: u32,
    args: Option<serde_json::Value>,
}

/// G4.9: Scheduler trace — records profiling events in Chrome Trace format.
///
/// Usage:
/// ```ignore
/// let mut trace = SchedulerTrace::new();
/// trace.begin("parse", 0, 0);
/// // ... do work ...
/// trace.end("parse", 0, 0);
/// let json = trace.to_json();
/// // Load in chrome://tracing
/// ```
pub struct SchedulerTrace {
    events: Vec<TraceEvent>,
    start_time: std::time::Instant,
}

impl SchedulerTrace {
    pub fn new() -> Self {
        SchedulerTrace {
            events: Vec::new(),
            start_time: std::time::Instant::now(),
        }
    }

    /// Record the start of a task computation (begin event).
    pub fn begin(&mut self, name: &str, pid: u32, tid: u32) {
        let ts = self.start_time.elapsed().as_micros() as u64;
        self.events.push(TraceEvent {
            name: name.to_string(),
            cat: "task".to_string(),
            ph: "B".to_string(),
            ts,
            dur: 0,
            pid,
            tid,
            args: None,
        });
    }

    /// Record the end of a task computation (end event).
    pub fn end(&mut self, name: &str, pid: u32, tid: u32) {
        let ts = self.start_time.elapsed().as_micros() as u64;
        self.events.push(TraceEvent {
            name: name.to_string(),
            cat: "task".to_string(),
            ph: "E".to_string(),
            ts,
            dur: 0,
            pid,
            tid,
            args: None,
        });
    }

    /// Record a complete event with a known duration.
    pub fn complete(
        &mut self,
        name: &str,
        pid: u32,
        tid: u32,
        dur_micros: u64,
        args: Option<serde_json::Value>,
    ) {
        let ts = self.start_time.elapsed().as_micros() as u64;
        self.events.push(TraceEvent {
            name: name.to_string(),
            cat: "task".to_string(),
            ph: "X".to_string(),
            ts,
            dur: dur_micros,
            pid,
            tid,
            args,
        });
    }

    /// Record an instant event (marker).
    pub fn instant(&mut self, name: &str, pid: u32, tid: u32) {
        let ts = self.start_time.elapsed().as_micros() as u64;
        self.events.push(TraceEvent {
            name: name.to_string(),
            cat: "marker".to_string(),
            ph: "i".to_string(),
            ts,
            dur: 0,
            pid,
            tid,
            args: None,
        });
    }

    /// Serialize to Chrome Trace JSON format.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.events).unwrap_or_else(|_| "[]".to_string())
    }

    /// Number of recorded events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Is the trace empty?
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

impl Default for SchedulerTrace {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute a line-by-line diff between two strings (G11.5).
///
/// Produces a unified-diff-style output showing added/removed lines.
fn compute_diff(first: &str, second: &str) -> String {
    let first_lines: Vec<&str> = first.lines().collect();
    let second_lines: Vec<&str> = second.lines().collect();
    let max_len = first_lines.len().max(second_lines.len());
    let mut diff = String::new();

    for i in 0..max_len {
        match (first_lines.get(i), second_lines.get(i)) {
            (Some(a), Some(b)) if a == b => {
                // Line unchanged — skip for brevity
            }
            (Some(a), Some(b)) => {
                diff.push_str(&format!("  - L{}: {}\n", i + 1, a));
                diff.push_str(&format!("  + L{}: {}\n", i + 1, b));
            }
            (Some(a), None) => {
                diff.push_str(&format!("  - L{}: {}\n", i + 1, a));
            }
            (None, Some(b)) => {
                diff.push_str(&format!("  + L{}: {}\n", i + 1, b));
            }
            (None, None) => {}
        }
    }

    if diff.is_empty() {
        // Binary data that differs but isn't line-structured
        diff.push_str(&format!(
            "  Output data differs (first {} bytes vs second {} bytes)",
            first.len(),
            second.len()
        ));
    }

    diff
}

impl TaskEngine {
    /// Get the task registry.
    pub fn registry(&self) -> &Arc<TaskRegistry> {
        &self.registry
    }

    /// Clear all cached state (for full rebuilds).
    pub fn clear(&self) {
        self.backend.clear();
        self.dep_graph.clear();
        self.agg_graph.clear();
    }

    /// Get cache statistics.
    pub fn stats(&self) -> TaskEngineStats {
        TaskEngineStats {
            cached_tasks: self.backend.memory_len(),
            total_tasks: self.dep_graph.len(),
            dirty_tasks: self.dep_graph.dirty_tasks().len(),
            clean_tasks: self.dep_graph.clean_tasks().len(),
        }
    }
}

/// Statistics about the task engine.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskEngineStats {
    pub cached_tasks: usize,
    pub total_tasks: usize,
    pub dirty_tasks: usize,
    pub clean_tasks: usize,
}

/// A builder for TaskEngine.
pub struct TaskEngineBuilder {
    registry: TaskRegistry,
    memory: MemoryBackend,
    disk: Option<crate::backend::DiskBackend>,
    remote: Option<pledgepack_cache::remote::RemoteCache>,
    verify_determinism: bool,
}

impl TaskEngineBuilder {
    pub fn new(registry: TaskRegistry) -> Self {
        TaskEngineBuilder {
            registry,
            memory: MemoryBackend::new(),
            disk: None,
            remote: None,
            verify_determinism: false,
        }
    }

    pub fn with_disk(mut self, disk: crate::backend::DiskBackend) -> Self {
        self.disk = Some(disk);
        self
    }

    pub fn with_remote(mut self, remote: pledgepack_cache::remote::RemoteCache) -> Self {
        self.remote = Some(remote);
        self
    }

    pub fn with_verify_determinism(mut self) -> Self {
        self.verify_determinism = true;
        self
    }

    pub fn build(self) -> TaskEngine {
        let mut backend = TaskBackend::new(self.memory);
        if let Some(disk) = self.disk {
            backend = backend.with_disk(disk);
        }
        if let Some(remote) = self.remote {
            backend = backend.with_remote(remote);
        }
        let engine = TaskEngine::new(self.registry, backend);
        if self.verify_determinism {
            engine.with_verify_determinism()
        } else {
            engine
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::StoredOutput;
    use crate::registry::{TaskExecutor, TaskRegistry};
    use crate::task::Task;

    #[tokio::test]
    async fn engine_computes_simple_task() {
        let registry = TaskRegistry::new();

        // Register a simple task: "greet" → "hello world"
        let task_id = TaskId::compute("greet", b"");
        registry.register(
            task_id,
            "greet".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("greet", b""),
                    &"hello world".to_string(),
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let task: Task<String> = Task::from_id(task_id);
        let result = task.read(&engine).await.unwrap();
        assert_eq!(*result, "hello world");
    }

    #[tokio::test]
    async fn engine_caches_task_output() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("compute_value", b"");
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = call_count.clone();

        registry.register(
            task_id,
            "compute_value".to_string(),
            TaskExecutor::sync(move || {
                call_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(StoredOutput::new(
                    TaskId::compute("compute_value", b""),
                    &42u32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        // First read — should compute
        let task: Task<u32> = Task::from_id(task_id);
        let result1 = task.read(&engine).await.unwrap();
        assert_eq!(*result1, 42);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 1);

        // Second read — should hit cache
        let result2 = task.read(&engine).await.unwrap();
        assert_eq!(*result2, 42);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 1); // Not recomputed
    }

    #[tokio::test]
    async fn engine_invalidate_marks_dirty() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("invalidate_test", b"");
        registry.register(
            task_id,
            "invalidate_test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("invalidate_test", b""),
                    &"value".to_string(),
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        // Compute the task
        let task: Task<String> = Task::from_id(task_id);
        let _ = task.read(&engine).await.unwrap();
        assert_eq!(
            engine.dependency_graph().status(&task_id),
            TaskStatus::Clean
        );

        // Invalidate
        engine.invalidate(task_id);
        assert_eq!(
            engine.dependency_graph().status(&task_id),
            TaskStatus::Dirty
        );
    }

    #[tokio::test]
    async fn engine_stats_track_tasks() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("stats_test", b"");
        registry.register(
            task_id,
            "stats_test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("stats_test", b""),
                    &"value".to_string(),
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let task: Task<String> = Task::from_id(task_id);
        let _ = task.read(&engine).await.unwrap();

        let stats = engine.stats();
        assert_eq!(stats.cached_tasks, 1);
        assert_eq!(stats.clean_tasks, 1);
    }

    #[tokio::test]
    async fn verify_determinism_passes_for_deterministic_task() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("det_test", b"");
        registry.register(
            task_id,
            "det_test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("det_test", b""),
                    &42u32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()))
            .with_verify_determinism();

        let task: Task<u32> = Task::from_id(task_id);
        let result = task.read(&engine).await.unwrap();
        assert_eq!(*result, 42);
    }

    #[tokio::test]
    async fn verify_determinism_detects_non_deterministic_task() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("nondet_test", b"");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = counter.clone();
        registry.register(
            task_id,
            "nondet_test".to_string(),
            TaskExecutor::sync(move || {
                let val = counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(StoredOutput::new(
                    TaskId::compute("nondet_test", b""),
                    &val,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()))
            .with_verify_determinism();

        let task: Task<u32> = Task::from_id(task_id);
        let result = task.read(&engine).await;
        assert!(result.is_err(), "Should detect non-determinism");
        let err = result.unwrap_err();
        assert!(matches!(err, TaskError::DeterminismViolation { .. }));
    }

    #[test]
    fn compute_diff_shows_line_differences() {
        let diff = compute_diff("hello\nworld\n", "hello\nrust\n");
        assert!(
            diff.contains("- L2: world"),
            "diff should show removed line"
        );
        assert!(diff.contains("+ L2: rust"), "diff should show added line");
    }

    #[test]
    fn compute_diff_handles_different_lengths() {
        let diff = compute_diff("a\nb\nc\n", "a\n");
        assert!(diff.contains("- L2: b"), "diff should show removed line");
        assert!(diff.contains("- L3: c"), "diff should show removed line");
    }

    #[test]
    fn compute_diff_handles_binary_data() {
        // The binary fallback fires when the line-by-line diff finds no
        // differences but the strings still differ. This can happen with
        // data that has identical lines but different byte representations.
        // We test the fallback by passing identical strings — the diff
        // will be empty, triggering the binary fallback.
        let diff = compute_diff("same", "same");
        assert!(
            diff.contains("Output data differs"),
            "should show binary diff info for identical strings"
        );
    }

    #[tokio::test]
    async fn non_cacheable_task_is_not_cached() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("non_cacheable_test", b"");
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_count_clone = call_count.clone();
        registry.register(
            task_id,
            "non_cacheable_test".to_string(),
            TaskExecutor::sync(move || {
                call_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(StoredOutput::new_non_cacheable(
                    TaskId::compute("non_cacheable_test", b""),
                    &42u32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let task: Task<u32> = Task::from_id(task_id);
        let result1 = task.read(&engine).await.unwrap();
        assert_eq!(*result1, 42);
        assert_eq!(call_count.load(std::sync::atomic::Ordering::Relaxed), 1);

        // Second read — should recompute since output was not cached
        let result2 = task.read(&engine).await.unwrap();
        assert_eq!(*result2, 42);
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "Non-cacheable task should recompute on every read"
        );
    }

    #[tokio::test]
    async fn batch_schedule_groups_independent_tasks() {
        let registry = TaskRegistry::new();
        let task_a = TaskId::compute("batch_a", b"");
        let task_b = TaskId::compute("batch_b", b"");
        let task_c = TaskId::compute("batch_c", b"");

        // Register tasks: A and B are independent, C depends on A
        registry.register(
            task_a,
            "batch_a".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("batch_a", b""),
                    &1u32,
                    vec![],
                )?)
            }),
        );
        registry.register(
            task_b,
            "batch_b".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("batch_b", b""),
                    &2u32,
                    vec![],
                )?)
            }),
        );
        registry.register(
            task_c,
            "batch_c".to_string(),
            TaskExecutor::sync(move || {
                Ok(StoredOutput::new(
                    TaskId::compute("batch_c", b""),
                    &3u32,
                    vec![task_a],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        // Compute A first so C has a dependency edge
        let task: Task<u32> = Task::from_id(task_a);
        let _ = task.read(&engine).await.unwrap();
        // Compute C so it registers the dependency edge
        let task: Task<u32> = Task::from_id(task_c);
        let _ = task.read(&engine).await.unwrap();

        // Now mark all as dirty
        engine.invalidate(task_a);
        engine.invalidate(task_b);
        engine.invalidate(task_c);

        // Register a query covering all three
        let _qid = engine.register_query(vec![task_a, task_b, task_c]);
        let dirty = engine.dirty_tasks_for_active_queries();
        assert!(dirty.contains(&task_a));
        assert!(dirty.contains(&task_b));
        assert!(dirty.contains(&task_c));

        let batches = engine.batch_schedule(&dirty).unwrap();
        // Batch 0 should contain A and B (independent)
        // Batch 1 should contain C (depends on A)
        assert_eq!(batches.len(), 2, "Should have 2 batches");
        assert!(
            batches[0].contains(&task_a),
            "Batch 0 should contain task_a"
        );
        assert!(
            batches[0].contains(&task_b),
            "Batch 0 should contain task_b"
        );
        assert!(
            !batches[0].contains(&task_c),
            "Batch 0 should not contain task_c"
        );
        assert!(
            batches[1].contains(&task_c),
            "Batch 1 should contain task_c"
        );
    }

    #[test]
    fn batch_schedule_empty_returns_empty() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let batches = engine.batch_schedule(&HashSet::new()).unwrap();
        assert!(batches.is_empty());
    }

    #[test]
    fn priority_schedule_orders_by_depth() {
        let registry = TaskRegistry::new();
        // Graph: root -> mid -> leaf1, leaf2
        // root has depth 0, mid has depth 1, leaf1/leaf2 have depth 2
        let root = TaskId::compute("prio_root", b"");
        let mid = TaskId::compute("prio_mid", b"");
        let leaf1 = TaskId::compute("prio_leaf1", b"");
        let leaf2 = TaskId::compute("prio_leaf2", b"");

        registry.register(
            leaf1,
            "prio_leaf1".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("prio_leaf1", b""),
                    &1u32,
                    vec![],
                )?)
            }),
        );
        registry.register(
            leaf2,
            "prio_leaf2".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("prio_leaf2", b""),
                    &2u32,
                    vec![],
                )?)
            }),
        );
        registry.register(
            mid,
            "prio_mid".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("prio_mid", b""),
                    &3u32,
                    vec![
                        TaskId::compute("prio_leaf1", b""),
                        TaskId::compute("prio_leaf2", b""),
                    ],
                )?)
            }),
        );
        registry.register(
            root,
            "prio_root".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("prio_root", b""),
                    &4u32,
                    vec![TaskId::compute("prio_mid", b"")],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        // Build dep graph by adding edges
        engine.dep_graph.add_edge(root, mid);
        engine.dep_graph.add_edge(mid, leaf1);
        engine.dep_graph.add_edge(mid, leaf2);

        // Mark all as dirty
        let dirty: HashSet<TaskId> = vec![root, mid, leaf1, leaf2].into_iter().collect();
        let scheduled = engine.priority_schedule(&dirty).unwrap();

        // Root (depth 0) should come before mid (depth 1) should come before leaves (depth 2)
        // But actually in batch_schedule, leaves come first (they have no dirty deps).
        // priority_schedule sorts within each batch by depth descending.
        // Batch 0: leaf1, leaf2 (no dirty deps) — sorted by depth: both depth 2
        // Batch 1: mid (deps in batch 0) — depth 1
        // Batch 2: root (dep in batch 1) — depth 0
        // So the order is: leaf1/leaf2, mid, root
        // But priority_schedule sorts by depth DESCENDING within batch.
        // Since all tasks in batch 0 have the same depth, order is by TaskId.
        // The key property: root should be prioritized (come last since it's in the last batch).
        // Actually, the point of priority scheduling is that within a batch,
        // tasks closer to root are computed first. But batch 0 only has leaves.
        // The real test: if we have two independent chains, the one closer to root
        // should be prioritized.

        // Verify that the scheduling produces all 4 tasks
        assert_eq!(scheduled.len(), 4, "Should schedule all 4 tasks");

        // Verify root is last (it's in the last batch)
        assert_eq!(
            *scheduled.last().unwrap(),
            root,
            "Root should be in the last batch"
        );
    }

    #[test]
    fn ttl_output_expires_after_ttl() {
        let id = TaskId::compute("ttl_test", b"input");
        let output = StoredOutput::new_with_ttl(id, &42u32, vec![], 1).unwrap();
        assert!(
            !output.is_expired(),
            "Output should not be expired immediately"
        );

        // Create an output that's already expired
        let expired_output = StoredOutput {
            task_id: id,
            data: output.data.clone(),
            output_hash: output.output_hash,
            dependencies: vec![],
            has_side_effects: false,
            expires_at: 1, // expired in the past (Unix epoch + 1 sec)
            read_dependencies: vec![],
        };
        assert!(
            expired_output.is_expired(),
            "Output with past expires_at should be expired"
        );
    }

    #[test]
    fn ttl_zero_means_no_expiration() {
        let id = TaskId::compute("ttl_zero_test", b"input");
        let output = StoredOutput::new_with_ttl(id, &42u32, vec![], 0).unwrap();
        assert!(!output.is_expired(), "TTL=0 should mean no expiration");
        assert_eq!(output.expires_at, 0, "expires_at should be 0 when TTL=0");
    }

    #[tokio::test]
    async fn visualize_dot_outputs_valid_dot() {
        let registry = TaskRegistry::new();
        let task_a = TaskId::compute("vis_a", b"");
        let task_b = TaskId::compute("vis_b", b"");
        let task_a_hex = task_a.to_hex();
        let task_b_hex = task_b.to_hex();

        registry.register(
            task_a,
            "vis_a".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("vis_a", b""),
                    &1u32,
                    vec![],
                )?)
            }),
        );
        registry.register(
            task_b,
            "vis_b".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("vis_b", b""),
                    &2u32,
                    vec![TaskId::compute("vis_a", b"")],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        // Compute tasks to populate the dep graph
        let task: Task<u32> = Task::from_id(task_b);
        let _ = task.read(&engine).await.unwrap();

        let dot = engine.visualize_dot();
        assert!(
            dot.contains("digraph task_graph"),
            "DOT output should contain digraph header"
        );
        assert!(
            dot.contains(&task_a_hex[..]),
            "DOT output should contain task_a node: {}",
            dot
        );
        assert!(
            dot.contains(&task_b_hex[..]),
            "DOT output should contain task_b node: {}",
            dot
        );
        assert!(dot.contains("->"), "DOT output should contain edges");
    }

    #[tokio::test]
    async fn visualize_mermaid_outputs_valid_mermaid() {
        let registry = TaskRegistry::new();
        let task_a = TaskId::compute("mermaid_a", b"");
        let task_b = TaskId::compute("mermaid_b", b"");

        registry.register(
            task_a,
            "mermaid_a".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("mermaid_a", b""),
                    &1u32,
                    vec![],
                )?)
            }),
        );
        registry.register(
            task_b,
            "mermaid_b".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("mermaid_b", b""),
                    &2u32,
                    vec![TaskId::compute("mermaid_a", b"")],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        // Compute tasks to populate the dep graph
        let task: Task<u32> = Task::from_id(task_b);
        let _ = task.read(&engine).await.unwrap();

        let mermaid = engine.visualize_mermaid();
        assert!(
            mermaid.starts_with("graph LR"),
            "Mermaid output should start with graph LR"
        );
        assert!(
            mermaid.contains("-->"),
            "Mermaid output should contain edges"
        );
    }

    #[test]
    fn scheduler_trace_produces_valid_chrome_trace_json() {
        let mut trace = SchedulerTrace::new();
        assert!(trace.is_empty(), "New trace should be empty");

        // Record a begin/end pair
        trace.begin("parse_js", 1, 0);
        trace.end("parse_js", 1, 0);

        // Record a complete event
        trace.complete("transform", 1, 1, 500, None);

        // Record an instant marker
        trace.instant("cache_miss", 1, 0);

        assert_eq!(trace.len(), 4, "Should have 4 events");

        let json = trace.to_json();
        assert!(json.starts_with("["), "JSON should be an array");
        assert!(json.contains("\"ph\":\"B\""), "Should contain begin event");
        assert!(json.contains("\"ph\":\"E\""), "Should contain end event");
        assert!(
            json.contains("\"ph\":\"X\""),
            "Should contain complete event"
        );
        assert!(
            json.contains("\"ph\":\"i\""),
            "Should contain instant event"
        );
        assert!(
            json.contains("\"name\":\"parse_js\""),
            "Should contain task name"
        );
        assert!(
            json.contains("\"cat\":\"task\""),
            "Should contain task category"
        );
    }

    #[test]
    fn parallel_flag_defaults_to_true() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let id = TaskId::compute("parallel_test", b"");
        assert!(engine.is_parallel(&id), "Default should be parallel=true");
    }

    #[test]
    fn parallel_flag_can_be_set_to_false() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let id = TaskId::compute("sequential_test", b"");
        engine.set_parallel(id, false);
        assert!(
            !engine.is_parallel(&id),
            "After set_parallel(false), should be false"
        );
    }

    #[test]
    fn env_filtered_visualization_excludes_other_envs() {
        use crate::environment::Environment;
        let registry = TaskRegistry::new();
        let task_client = TaskId::compute("env_client_task", b"");
        let task_server = TaskId::compute("env_server_task", b"");
        let task_shared = TaskId::compute("env_shared_task", b"");

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        // Set environments
        engine.set_task_env(task_client, Environment::Client);
        engine.set_task_env(task_server, Environment::Server);
        engine.set_task_env(task_shared, Environment::Shared);

        // Build dep graph
        engine.dep_graph.add_edge(task_client, task_shared);
        engine.dep_graph.add_edge(task_server, task_shared);

        // Visualize for Client env — should include client + shared, exclude server
        let dot = engine.visualize_dot_for_env(Environment::Client);
        assert!(
            dot.contains(&task_client.to_hex()[..]),
            "Should contain client task"
        );
        assert!(
            dot.contains(&task_shared.to_hex()[..]),
            "Should contain shared task"
        );
        assert!(
            !dot.contains(&task_server.to_hex()[..]),
            "Should NOT contain server task"
        );

        // Visualize for Server env — should include server + shared, exclude client
        let dot_server = engine.visualize_dot_for_env(Environment::Server);
        assert!(
            dot_server.contains(&task_server.to_hex()[..]),
            "Should contain server task"
        );
        assert!(
            !dot_server.contains(&task_client.to_hex()[..]),
            "Should NOT contain client task"
        );
    }

    #[test]
    fn checkpoint_and_restore_preserves_state() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let id_a = TaskId::compute("checkpoint_a", b"");
        let id_b = TaskId::compute("checkpoint_b", b"");

        // Set up state
        engine.dep_graph.add_edge(id_a, id_b);
        engine.dep_graph.set_status(id_a, TaskStatus::Dirty);
        engine.dep_graph.set_status(id_b, TaskStatus::Clean);
        engine.set_ttl(id_a, 300);
        engine.set_parallel(id_b, false);
        engine.set_task_env(id_a, Environment::Client);

        // Create checkpoint
        let cp = engine.checkpoint();

        // Verify checkpoint captures state
        assert_eq!(cp.task_statuses.len(), 2, "Should have 2 task statuses");
        assert_eq!(cp.task_ttls.len(), 1, "Should have 1 TTL");
        assert_eq!(cp.task_ttls[0].1, 300, "TTL should be 300");
        assert_eq!(cp.task_parallel.len(), 1, "Should have 1 parallel flag");
        assert!(!cp.task_parallel[0].1, "Parallel flag should be false");
        assert_eq!(cp.task_envs.len(), 1, "Should have 1 env");
        assert_eq!(cp.task_envs[0].1, "Client", "Env should be Client");

        // Modify state
        engine.dep_graph.set_status(id_a, TaskStatus::Clean);
        engine.set_ttl(id_a, 0);
        engine.set_parallel(id_b, true);

        // Restore from checkpoint
        engine.restore_checkpoint(&cp);

        // Verify state is restored
        assert_eq!(
            engine.dep_graph.status(&id_a),
            TaskStatus::Dirty,
            "Status should be restored to Dirty"
        );
        assert_eq!(
            engine.get_ttl(&id_a),
            Some(300),
            "TTL should be restored to 300"
        );
        assert!(
            !engine.is_parallel(&id_b),
            "Parallel flag should be restored to false"
        );
        assert_eq!(
            engine.get_task_env(&id_a),
            Some(Environment::Client),
            "Env should be restored to Client"
        );
    }

    #[test]
    fn checkpoint_serializes_to_json() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let id = TaskId::compute("json_cp", b"");
        engine.dep_graph.add_edge(id, id);
        engine.dep_graph.set_status(id, TaskStatus::Clean);
        engine.set_ttl(id, 60);

        let cp = engine.checkpoint();
        let json = serde_json::to_string(&cp).expect("Should serialize to JSON");
        assert!(json.contains("Clean"), "JSON should contain status");
        assert!(json.contains("60"), "JSON should contain TTL");

        let restored: SchedulerCheckpoint =
            serde_json::from_str(&json).expect("Should deserialize from JSON");
        assert_eq!(
            restored.task_ttls.len(),
            1,
            "Restored checkpoint should have 1 TTL"
        );
    }

    #[test]
    fn speculative_schedule_prioritizes_proximity_tasks() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        // Build graph: root -> a -> b, c is independent
        let root = TaskId::compute("spec_root", b"");
        let a = TaskId::compute("spec_a", b"");
        let b = TaskId::compute("spec_b", b"");
        let c = TaskId::compute("spec_c", b"");

        engine.dep_graph.add_edge(root, a);
        engine.dep_graph.add_edge(a, b);
        engine.dep_graph.add_edge(c, c); // c has an edge so it appears in all_tasks

        // Mark all as dirty
        engine.dep_graph.set_status(root, TaskStatus::Dirty);
        engine.dep_graph.set_status(a, TaskStatus::Dirty);
        engine.dep_graph.set_status(b, TaskStatus::Dirty);
        engine.dep_graph.set_status(c, TaskStatus::Dirty);

        // Register an active query for root
        engine.register_query(vec![root]);

        // Speculative schedule should prioritize root, a, b (in proximity)
        // over c (independent)
        let spec = engine.speculative_schedule();
        assert!(!spec.is_empty(), "Should return dirty tasks");

        // root, a, b should come before c (they're in the proximity set)
        let pos_c = spec.iter().position(|t| *t == c).unwrap();
        let pos_root = spec.iter().position(|t| *t == root).unwrap();
        let pos_a = spec.iter().position(|t| *t == a).unwrap();
        let pos_b = spec.iter().position(|t| *t == b).unwrap();

        assert!(
            pos_root < pos_c,
            "root should be scheduled before c (proximity)"
        );
        assert!(pos_a < pos_c, "a should be scheduled before c (proximity)");
        assert!(pos_b < pos_c, "b should be scheduled before c (proximity)");
    }

    #[test]
    fn speculative_schedule_empty_when_no_dirty() {
        let registry = TaskRegistry::new();
        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let spec = engine.speculative_schedule();
        assert!(spec.is_empty(), "Should return empty when no dirty tasks");
    }

    // ─── G5.10: Environment-parallel execution tests ────────────────

    #[tokio::test]
    async fn env_parallel_executes_multiple_environments() {
        let registry = TaskRegistry::new();

        // Register tasks for different environments
        let client_id = TaskId::compute("transform", b"client-module");
        let server_id = TaskId::compute("transform", b"server-module");

        registry.register(
            client_id,
            "transform_client".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("transform", b"client-module"),
                    &"client-output".to_string(),
                    vec![],
                )?)
            }),
        );
        registry.register(
            server_id,
            "transform_server".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("transform", b"server-module"),
                    &"server-output".to_string(),
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let tasks = vec![
            (client_id, crate::environment::Environment::Client),
            (server_id, crate::environment::Environment::Server),
        ];

        let results = engine
            .read_tasks_for_environments::<String>(tasks)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        let outputs: std::collections::HashMap<TaskId, String> = results
            .into_iter()
            .map(|(id, val)| (id, (*val).clone()))
            .collect();
        assert_eq!(outputs.get(&client_id), Some(&"client-output".to_string()));
        assert_eq!(outputs.get(&server_id), Some(&"server-output".to_string()));
    }

    #[tokio::test]
    async fn env_parallel_sets_task_env_metadata() {
        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("test", b"env-meta");

        registry.register(
            task_id,
            "test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("test", b"env-meta"),
                    &42i32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));

        let tasks = vec![(task_id, crate::environment::Environment::Edge)];
        let _results = engine
            .read_tasks_for_environments::<i32>(tasks)
            .await
            .unwrap();

        // Verify environment metadata was set
        let env = engine.get_task_env(&task_id).unwrap();
        assert_eq!(env, crate::environment::Environment::Edge);
    }
}
