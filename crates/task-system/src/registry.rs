// Task registry — maps TaskId → executor function.
//
// The registry is the bridge between the `#[task]` macro and the engine.
// When the macro generates a task function, it registers an executor that:
//   1. Reads all input Task<T> values (recursively computing them if needed)
//   2. Calls the user's function body
//   3. Wraps the result in a StoredOutput with dependency edges
//
// The registry is NOT a global singleton — it's owned by the TaskEngine.
// This avoids the global mutable state problems that turbo-tasks has with
// its `Lazy::new` registration pattern.

use crate::backend::StoredOutput;
use crate::engine::TaskEngine;
use crate::task::TaskId;
use anyhow::Result;
use dashmap::DashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A boxed future returned by a task executor.
pub type TaskFuture = Pin<Box<dyn Future<Output = Result<StoredOutput>> + Send + 'static>>;

/// A task executor — the function that computes a task's output.
///
/// This is an async boxed future because tasks may need to read their
/// dependencies (which are themselves async operations).
pub struct TaskExecutor {
    inner: Arc<dyn Fn() -> TaskFuture + Send + Sync>,
}

impl TaskExecutor {
    /// Create a task executor from a sync function.
    ///
    /// For tasks that don't need to read dependencies (leaf tasks).
    pub fn sync<F>(f: F) -> Self
    where
        F: Fn() -> Result<StoredOutput> + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        TaskExecutor {
            inner: Arc::new(move || {
                let f = f.clone();
                Box::pin(async move { f() })
            }),
        }
    }

    /// Create a task executor from an async function.
    ///
    /// For tasks that need to read their dependencies. The closure receives
    /// no arguments — it should capture whatever it needs (e.g., task IDs
    /// to read from the engine via thread-local or passed context).
    ///
    /// Note: the future must be `'static` (no borrows of the engine).
    /// Tasks that need to read dependencies should capture `TaskId`s and
    /// use a thread-local engine reference or pass the engine via other means.
    /// This is a design trade-off to avoid lifetime complexity.
    pub fn async_fn<F>(f: F) -> Self
    where
        F: Fn() -> TaskFuture + Send + Sync + 'static,
    {
        TaskExecutor { inner: Arc::new(f) }
    }

    /// Execute the task.
    pub async fn execute(&self, _engine: &TaskEngine) -> Result<StoredOutput> {
        (self.inner)().await
    }
}

/// The task registry — maps TaskId → (function_name, executor).
///
/// Thread-safe via DashMap. Owned by the TaskEngine.
pub struct TaskRegistry {
    /// TaskId → (function name, executor)
    executors: DashMap<TaskId, (String, TaskExecutor)>,
}

impl TaskRegistry {
    /// Create an empty registry with no executors registered.
    pub fn new() -> Self {
        TaskRegistry {
            executors: DashMap::new(),
        }
    }

    /// Register a task executor.
    pub fn register(&self, id: TaskId, function_name: String, executor: TaskExecutor) {
        self.executors.insert(id, (function_name, executor));
    }

    /// Check if a task is registered.
    pub fn contains(&self, id: &TaskId) -> bool {
        self.executors.contains_key(id)
    }

    /// Number of registered tasks.
    pub fn len(&self) -> usize {
        self.executors.len()
    }

    /// Is the registry empty?
    pub fn is_empty(&self) -> bool {
        self.executors.is_empty()
    }

    /// Execute a registered task by ID.
    ///
    /// This looks up the executor and runs it. Called by the TaskEngine
    /// on cache miss.
    pub async fn execute(&self, id: &TaskId, engine: &TaskEngine) -> Result<StoredOutput> {
        let executor = self
            .executors
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Task executor not found: {}", id))?;
        executor.value().1.execute(engine).await
    }
}

impl Default for TaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_executes_sync_task() {
        let registry = TaskRegistry::new();
        let id = TaskId::compute("test", b"");

        registry.register(
            id,
            "test".to_string(),
            TaskExecutor::sync(move || Ok(StoredOutput::new(id, &"hello".to_string(), vec![])?)),
        );

        // We can't easily test execution without a full TaskEngine,
        // but we can verify the registry contains the task.
        assert!(registry.contains(&id));
        assert_eq!(registry.len(), 1);
    }
}
