//! pledgepack-task-system — PledgePack's incremental computation engine.
//!
//! This is PledgePack's own task graph system, designed from scratch to avoid
//! the structural problems of Turbopack's turbo-tasks (see TURBO_TASKS_ANALYSIS.md).
//!
//! # Design Principles
//!
//! 1. **One core type**: `Task<T>` — no `ResolvedVc`, `RawVc`, `OperationVc`, etc.
//! 2. **Content-addressed**: `TaskId = blake3(function_id ++ input_hashes ++ environment)` — the
//!    task ID IS the cache key. Environment-aware (G5.1).
//! 3. **Explicit dependencies**: `Task<T>` arguments are the task's dependencies.
//!    Supplemented by opt-in read tracking (deterministic, thread-local).
//! 4. **Stable Rust**: zero nightly features (turbo-tasks needs 10).
//! 5. **serde for serialization**: no custom bincode traits, no `DeterministicHash`.
//! 6. **No read consistency modes**: all reads are consistent by construction.
//! 7. **No cell modes**: content hash is the invalidation signal (0 config vs 12).
//! 8. **Demand-driven scheduler**: defer dirty task re-execution until an active
//!    query covers them.
//! 9. **Arena-friendly**: graph storage is contiguous arrays, ready for Zig arena.
//! 10. **WASM-first task boundary** (future): the task boundary will be the WIT
//!     contract — Rust functions, WASM plugins, and JS plugins all treated uniformly.
//! 11. **No collectibles** (v0.1): explicit aggregation tasks instead.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        TaskEngine                            │
//! │  ┌─────────────┐  ┌──────────────┐  ┌────────────────────┐  │
//! │  │ TaskBackend │  │ Dependency   │  │ Aggregation        │  │
//! │  │ (3-tier)    │  │ Graph        │  │ Graph (O(log n))   │  │
//! │  │ mem→disk→   │  │ (edges,      │  │ (subtree status    │  │
//! │  │  remote     │  │  invalidation│  │  summarization)    │  │
//! │  └─────────────┘  └──────────────┘  └────────────────────┘  │
//! │  ┌──────────────────────────────────────────────────────┐   │
//! │  │ TaskRegistry (TaskId → executor)                     │   │
//! │  │ Demand-driven scheduler (active queries)             │   │
//! │  └──────────────────────────────────────────────────────┘   │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Usage
//!
//! ```ignore
//! use pledgepack_task_system::{Task, TaskId, TaskEngine, TaskEngineBuilder, TaskRegistry, TaskExecutor};
//!
//! // 1. Create a registry and register tasks
//! let registry = TaskRegistry::new();
//! let task_id = TaskId::compute("greet", b"");
//! registry.register(task_id, "greet".to_string(), TaskExecutor::sync(|| {
//!     Ok(StoredOutput::new(task_id, &"hello world".to_string(), vec![])?)
//! }));
//!
//! // 2. Build the engine
//! let engine = TaskEngineBuilder::new(registry).build();
//!
//! // 3. Read task outputs
//! let task: Task<String> = Task::from_id(task_id);
//! let result = task.read(&engine).await?; // "hello world"
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

/// 3-tier task output storage (memory → disk → remote).
pub mod backend;
/// The `TaskEngine` — schedules, executes, and caches tasks on demand.
pub mod engine;
/// Execution environment (browser/server/shared) used for environment-aware task IDs.
pub mod environment;
/// The dependency graph: edges between tasks and dirty/invalidation propagation.
pub mod graph;
/// Thread-local read tracking for implicit dependency capture.
pub mod read_tracker;
/// The `TaskId` → executor registry.
pub mod registry;
/// Route-based prefetch tracking and prediction.
pub mod route_tracker;
/// The core `Task<T>` type and `TaskId` computation.
pub mod task;
#[cfg(feature = "task-trace")]
pub mod task_trace;
/// Zig arena-backed dependency graph — the default `TaskGraphOps` backend.
#[cfg(feature = "zig-graph")]
pub mod zig_graph;

// Re-export the most commonly used types at the crate root.
pub use backend::{CasBackend, DiskBackend, DiskTier, MemoryBackend, StoredOutput, TaskBackend};
pub use engine::{
    ActiveQuery, SchedulerTrace, TaskEngine, TaskEngineBuilder, TaskEngineStats, TaskError,
};
pub use environment::{
    Environment, EnvironmentPlugin, EnvironmentPluginRegistry, current_environment,
    run_with_environment,
};
pub use graph::{
    AggregationGraph, AggregationNode, DependencyGraph, TaskGraphOps, TaskStatus, default_dep_graph,
};
pub use read_tracker::{
    ReadTracker, collect_tracker, install_tracker, is_tracking, read as tracked_read,
    read_to_string as tracked_read_to_string, record_read,
};
pub use registry::{TaskExecutor, TaskRegistry};
pub use route_tracker::{RouteEntry, RouteTracker, RouteTrackerConfig};
pub use task::{
    AnyTask, HasEffect, NoEffect, Task, TaskDebug, TaskEffect, TaskId, TaskInput, TaskVerify,
    TaskVersion, V1, V2, compute_task_id, compute_task_id_compact,
};
#[cfg(feature = "zig-graph")]
pub use zig_graph::ZigTaskGraph;

// Re-export the proc macro.
pub use pledgepack_task_system_macros::task;
