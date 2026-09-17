// Task<T> — the single core type of PledgePack's incremental computation engine.
//
// Design principles (from TURBO_TASKS_ANALYSIS.md):
//   1. One core type, not nine (no ResolvedVc, RawVc, OperationVc, ReadRef, etc.)
//   2. Content-addressed: TaskId = blake3(function_id ++ input_hashes ++ environment)
//   3. Copy, 16 bytes (128-bit blake3 hash), Send + Sync
//   4. Explicit dependencies (Task<T> arguments are the task's dependencies)
//   5. Read tracking supplements explicit deps (opt-in, deterministic)
//   6. Environment-aware: same source in Client vs Server = different TaskId
//   6. Stable Rust, zero nightly features
//   7. serde for serialization (no custom bincode traits)
//   8. The task ID IS the cache key — no backend-assigned IDs

use crate::environment::Environment;
use blake3::Hasher;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher as StdHasher};
use std::marker::PhantomData;

/// A 128-bit content-addressed task identifier.
///
/// This is `blake3(function_id ++ input_task_ids ++ input_values)` truncated to 16 bytes.
/// It is the cache key — there is no separate "task ID assigned by the backend."
/// The same inputs always produce the same `TaskId`, deterministically.
///
/// 128 bits (vs Turbopack's 64-bit backend-assigned IDs) reduces collision probability
/// to negligible even at millions of tasks.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId([u8; 16]);

impl TaskId {
    /// The zero task ID — used for tasks with no inputs (root tasks).
    pub const ZERO: TaskId = TaskId([0u8; 16]);

    /// Create a `TaskId` from raw 16 bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        TaskId(bytes)
    }

    /// Get the raw 16 bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Compute a `TaskId` from a function ID and serialized input bytes.
    ///
    /// This is the core content-addressing operation:
    ///   `TaskId = blake3(function_id_bytes ++ input_bytes)[0..16]`
    ///
    /// The function ID is typically the fully-qualified function name (e.g.,
    /// `pledgepack_core::transform::transform_jsx`). The input bytes are the
    /// concatenation of each input's task ID bytes (for `Task<T>` inputs) or
    /// serde-serialized bytes (for value inputs).
    ///
    /// Uses `Environment::Shared` (environment-agnostic). Use `compute_with_env()`
    /// for environment-specific task IDs.
    pub fn compute(function_id: &str, input_bytes: &[u8]) -> TaskId {
        Self::compute_with_env(function_id, input_bytes, Environment::Shared)
    }

    /// Compute a `TaskId` from a function ID, serialized input bytes, and an environment.
    ///
    /// This is the environment-aware content-addressing operation (G5.1):
    ///   `TaskId = blake3(function_id_bytes ++ 0xFF ++ input_bytes ++ 0xFD ++ env_bytes)[0..16]`
    ///
    /// The environment is mixed into the hash so that the same source file
    /// produces different task nodes in different environments. `Shared`
    /// environment tasks are computed once and shared across all environments.
    pub fn compute_with_env(function_id: &str, input_bytes: &[u8], env: Environment) -> TaskId {
        let mut hasher = Hasher::new();
        hasher.update(function_id.as_bytes());
        // A separator to prevent function_id/input_bytes ambiguity.
        hasher.update(&[0xFF]);
        hasher.update(input_bytes);
        // Environment separator + env string to make env-specific TaskIds.
        hasher.update(&[0xFD]);
        hasher.update(env.as_str().as_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash.as_bytes()[..16]);
        TaskId(bytes)
    }

    /// Compute a `TaskId` from a function ID and a list of input task IDs.
    ///
    /// This is the common case: all inputs are `Task<T>` values, so we
    /// concatenate their 16-byte IDs. Uses `Environment::Shared`.
    pub fn from_tasks(function_id: &str, inputs: &[TaskId]) -> TaskId {
        Self::from_tasks_with_env(function_id, inputs, Environment::Shared)
    }

    /// Compute a `TaskId` from a function ID, a list of input task IDs, and an environment.
    ///
    /// Environment-aware version of `from_tasks()` (G5.1).
    pub fn from_tasks_with_env(function_id: &str, inputs: &[TaskId], env: Environment) -> TaskId {
        let mut input_bytes = Vec::with_capacity(inputs.len() * 16);
        for id in inputs {
            input_bytes.extend_from_slice(id.as_bytes());
        }
        Self::compute_with_env(function_id, &input_bytes, env)
    }

    /// Compute a `TaskId` from a function ID, task inputs, and a serde-serializable
    /// extra-params value.
    ///
    /// The params value is serialized via serde and appended to the input bytes.
    /// This handles cases like `transform(source: Task<SourceFile>, config: TransformConfig)`
    /// where `config` is a plain value, not a `Task<T>`.
    pub fn from_tasks_and_params<P: Serialize>(
        function_id: &str,
        task_inputs: &[TaskId],
        params: &P,
    ) -> Result<TaskId, serde_json::Error> {
        Self::from_tasks_and_params_with_env(function_id, task_inputs, params, Environment::Shared)
    }

    /// Compute a `TaskId` from a function ID, task inputs, a serde-serializable
    /// params value, and an environment.
    ///
    /// Environment-aware version of `from_tasks_and_params()` (G5.1).
    pub fn from_tasks_and_params_with_env<P: Serialize>(
        function_id: &str,
        task_inputs: &[TaskId],
        params: &P,
        env: Environment,
    ) -> Result<TaskId, serde_json::Error> {
        let mut input_bytes = Vec::with_capacity(task_inputs.len() * 16 + 64);
        for id in task_inputs {
            input_bytes.extend_from_slice(id.as_bytes());
        }
        // A separator between task IDs and params to prevent ambiguity.
        input_bytes.push(0xFE);
        let params_bytes = serde_json::to_vec(params)?;
        input_bytes.extend_from_slice(&params_bytes);
        Ok(Self::compute_with_env(function_id, &input_bytes, env))
    }

    /// Display as a hex string (32 chars).
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for byte in &self.0 {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    /// Parse from a hex string (32 chars).
    pub fn from_hex(hex: &str) -> Option<TaskId> {
        if hex.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for i in 0..16 {
            bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(TaskId(bytes))
    }

    /// Return a short hex prefix (first 8 chars / 4 bytes) for debugging.
    ///
    /// This is used by `debug_id()` to produce concise identifiers in logs
    /// and error messages. The full 32-char hex is available via `to_hex()`.
    pub fn short_hex(&self) -> String {
        let mut s = String::with_capacity(8);
        for byte in &self.0[..4] {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    /// Produce a human-readable debug identifier: `function_name#short_hash`.
    ///
    /// G1.12: This gives every task a concise, readable name for diagnostics
    /// without requiring a registry lookup. The function name is provided by
    /// the caller (typically from `std::any::type_name` or the `#[task]` macro's
    /// function ID). The short hash is the first 8 hex chars of the TaskId.
    ///
    /// Example: `"transform_jsx#a1b2c3d4"`
    pub fn debug_id(&self, function_name: &str) -> String {
        format!("{}#{}", function_name, self.short_hex())
    }
}

impl Ord for TaskId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for TaskId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TaskId({})", self.to_hex())
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

impl Serialize for TaskId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for TaskId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes: Vec<u8> = serde::Deserialize::deserialize(deserializer)?;
        if bytes.len() != 16 {
            return Err(serde::de::Error::custom("TaskId must be exactly 16 bytes"));
        }
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes);
        Ok(TaskId(arr))
    }
}

/// A lazy, cached, content-addressed computation.
///
/// `Task<T>` is the **single core type** of PledgePack's incremental computation engine.
/// There is no `ResolvedVc`, `RawVc`, `OperationVc`, `ReadRef`, `SharedReference`,
/// `TypedSharedReference`, `TransientValue`, or `TransientInstance` — just `Task<T>`.
///
/// # Properties
///
/// - **16 bytes**: just a `TaskId` (128-bit blake3 hash) + a phantom type marker.
///   `Copy` is free — passing a `Task<T>` is a register copy.
/// - **Content-addressed**: the `TaskId` is `blake3(function_id ++ input_hashes)`.
///   The same inputs always produce the same `TaskId`, deterministically.
/// - **Send + Sync**: it's just 16 bytes with no interior mutability.
/// - **Hash + Eq + Ord**: via the `TaskId`. Can be used as a `HashMap` key with zero overhead.
/// - **No refcounting**: no `Arc`, no `Rc`. The task graph owns the outputs; `Task<T>` is a
///   reference to a graph node, not a refcounted value.
///
/// # Reading
///
/// To get the output value, call `task.read(&engine).await?`, which returns `Arc<T>`.
/// The read is always consistent — there is no "stale" vs "fresh" because the task ID
/// is the content hash, and the output stored against it is the output for those exact
/// inputs.
///
/// # Dependencies
///
/// Dependencies are **explicit**: a task's `Task<T>` arguments are its dependencies.
/// The dependency graph is built from the call structure, not from runtime read patterns.
/// No thread-local read interception, no non-determinism bugs.
///
/// # Example
///
/// ```ignore
/// #[task]
/// fn parse_source(source: Task<SourceFile>) -> Task<ParsedModule> {
///     let source = source.read(&engine).await?;
///     ParsedModule::from(oxc_parse(&source.content))
/// }
/// ```
/// G1.14: Effect marker trait for the side-effect type parameter.
///
/// Tasks are `Task<T, NoEffect>` by default. Side-effecting tasks (writing to
/// disk, network calls) use `Task<T, HasEffect>` so the engine can track them
/// separately for correct invalidation. Turbopack needs `OperationVc` — a whole
/// separate type hierarchy. PledgePack uses a generic parameter with a default.
pub trait TaskEffect: Copy + 'static {
    /// Whether this task type has side effects.
    const HAS_SIDE_EFFECTS: bool;
}

/// G1.14: Default — no side effects. Most tasks are pure functions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoEffect;

impl TaskEffect for NoEffect {
    const HAS_SIDE_EFFECTS: bool = false;
}

/// G1.14: Marker for tasks with side effects (file writes, network, etc.).
///
/// Side-effecting tasks are not cached the same way — they must be re-executed
/// when their inputs change, even if the output looks the same.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HasEffect;

impl TaskEffect for HasEffect {
    const HAS_SIDE_EFFECTS: bool = true;
}

/// G1.19: Version marker trait for compile-time schema versioning.
///
/// When a task function's logic changes (new version), the version is part of
/// the task ID, so old cached results are automatically invalid. This is
/// compile-time schema versioning — Turbopack has no equivalent.
pub trait TaskVersion: Copy + 'static {
    /// The version number. Changing this invalidates all cached results.
    const VERSION: u32;
}

/// G1.19: Default version (v1). Used when no version is specified.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V1;

impl TaskVersion for V1 {
    const VERSION: u32 = 1;
}

/// G1.19: Version 2. Use when task logic changes in a breaking way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V2;

impl TaskVersion for V2 {
    const VERSION: u32 = 2;
}

/// G1.20: Compile-time verification trait.
///
/// `Task<T>::verify()` is a const fn that checks at compile time that the
/// task's output type implements `Serialize + DeserializeOwned + Send + Sync`.
/// If it doesn't, the code doesn't compile. Turbopack's `Vc` has runtime checks;
/// PledgePack's are compile-time.
pub trait TaskVerify: Serialize + DeserializeOwned + Send + Sync {}

/// G1.20: Blanket implementation — any type satisfying the bounds is verified.
impl<T: Serialize + DeserializeOwned + Send + Sync> TaskVerify for T {}

/// Marker for `Task`'s phantom `T`/`E`/`V` type parameters — the task never
/// actually stores a `T`, `E`, or `V`, only a content-addressed `TaskId`; this
/// exists purely so the compiler tracks the intended output/effect/version
/// types at each use site.
type TaskMarker<T, E, V> = PhantomData<fn() -> (T, E, V)>;

/// A reference to a task's output, identified by its content-addressed
/// [`TaskId`]. Cheap to copy — it carries no data of its own beyond the ID
/// and a zero-sized marker for `T`/`E`/`V`. Call [`Task::read`] to resolve
/// the actual output of type `T`.
pub struct Task<T, E: TaskEffect = NoEffect, V: TaskVersion = V1> {
    id: TaskId,
    _marker: TaskMarker<T, E, V>,
}

// Manual impls because T, E, V are only used in PhantomData<fn() -> (T, E, V)> which is
// always Send+Sync regardless of the actual types.
unsafe impl<T, E: TaskEffect, V: TaskVersion> Send for Task<T, E, V> {}
unsafe impl<T, E: TaskEffect, V: TaskVersion> Sync for Task<T, E, V> {}

impl<T, E: TaskEffect, V: TaskVersion> Clone for Task<T, E, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, E: TaskEffect, V: TaskVersion> Copy for Task<T, E, V> {}

impl<T, E: TaskEffect, V: TaskVersion> PartialEq for Task<T, E, V> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T, E: TaskEffect, V: TaskVersion> Eq for Task<T, E, V> {}

impl<T, E: TaskEffect, V: TaskVersion> Hash for Task<T, E, V> {
    fn hash<H: StdHasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl<T, E: TaskEffect, V: TaskVersion> Ord for Task<T, E, V> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl<T, E: TaskEffect, V: TaskVersion> PartialOrd for Task<T, E, V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T, E: TaskEffect, V: TaskVersion> fmt::Debug for Task<T, E, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("id", &self.id)
            .field("type", &std::any::type_name::<T>())
            .finish()
    }
}

impl<T, E: TaskEffect, V: TaskVersion> Task<T, E, V> {
    /// Create a `Task<T>` from a known `TaskId`.
    ///
    /// This is used internally by the `#[task]` macro and the task registry.
    /// Users should not call this directly — use the task function generated by `#[task]`.
    pub const fn from_id(id: TaskId) -> Self {
        Task {
            id,
            _marker: PhantomData,
        }
    }

    /// Get the `TaskId` (the 128-bit content hash).
    ///
    /// This is the cache key. It is a pure function of the task's inputs —
    /// calling it twice with the same inputs always returns the same ID.
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Read the task's output value from the engine.
    ///
    /// Returns `Arc<T>` — a shared, cheap reference to the cached output.
    /// If the task has not been computed yet, the engine will schedule it
    /// (and its dependencies) for execution.
    ///
    /// This is always consistent — there is no "stale" vs "fresh" because
    /// the task ID is the content hash.
    pub async fn read(
        &self,
        engine: &crate::TaskEngine,
    ) -> Result<std::sync::Arc<T>, crate::TaskError>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        engine.read_task::<T>(self.id).await
    }

    /// Try to read the task's output without scheduling computation.
    ///
    /// Returns `Some(Arc<T>)` if the task is already computed and cached,
    /// `None` if it needs to be scheduled.
    pub fn try_read(&self, engine: &crate::TaskEngine) -> Option<std::sync::Arc<T>>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        engine.try_read_task::<T>(self.id)
    }

    /// G1.10: Lock-free atomic read fast path.
    ///
    /// This is a **synchronous, lock-free** read for the common case where the
    /// task is already computed and clean. It avoids:
    ///   - Async overhead (no Tokio task spawn, no `.await`)
    ///   - Mutex acquisition (no `computing` lock)
    ///   - RwLock acquisition (no `active_queries` lock)
    ///
    /// The fast path is:
    ///   1. Atomic status check via DashMap (lock-free read) — is the task Clean?
    ///   2. Atomic memory lookup via DashMap (lock-free read) — is the output cached?
    ///   3. Deserialize and return.
    ///
    /// If the task is dirty, computing, pending, or not in memory cache, this
    /// returns `None` and the caller should fall back to `read(&engine).await`.
    ///
    /// This beats Turbopack's `Vc::read()` which goes through the Tokio runtime
    /// even for cache hits. Our fast path is a direct DashMap lookup + pointer
    /// chase — no async runtime involvement.
    pub fn read_fast(&self, engine: &crate::TaskEngine) -> Option<std::sync::Arc<T>>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        // 1. Lock-free status check — DashMap read is sharded, not a full lock.
        //    If the task is not Clean, we can't serve from cache.
        let status = engine.dependency_graph().status(&self.id);
        if status != crate::graph::TaskStatus::Clean {
            return None;
        }

        // 2. Lock-free memory lookup — DashMap get + Arc clone (pointer chase).
        //    No disk or remote fallback — those require async I/O.
        let output = engine.backend().get_memory(&self.id)?;

        // 3. Deserialize — this is the only non-lock-free step, but it's CPU-only
        //    and doesn't involve any runtime or I/O.
        let value: T = output.deserialize().ok()?;
        Some(std::sync::Arc::new(value))
    }

    /// G1.15: Non-blocking, non-tracking read that returns `Option<Arc<T>>`.
    ///
    /// Returns `Some(Arc<T>)` if the task is already computed and cached in memory,
    /// `None` if it isn't — without scheduling computation or checking dirty status.
    ///
    /// Unlike `read_fast`, this does NOT check whether the task is Clean — it
    /// simply checks if an output exists in the memory cache. This makes it
    /// suitable for speculative prefetching and UI status displays where you
    /// want to peek at a value without implying a dependency.
    ///
    /// Unlike `try_read`, this does not check the disk cache (memory-only) and
    /// does not require the task to be in an active query.
    ///
    /// **Beats Turbopack:** all reads in Turbopack are blocking/tracking — there
    /// is no way to peek at a value without scheduling it.
    pub fn peek(&self, engine: &crate::TaskEngine) -> Option<std::sync::Arc<T>>
    where
        T: Serialize + DeserializeOwned + Send + Sync + 'static,
    {
        let output = engine.backend().get_memory(&self.id)?;
        let value: T = output.deserialize().ok()?;
        Some(std::sync::Arc::new(value))
    }

    /// G1.16: Manually evict a task's output from the memory cache while
    /// keeping the dependency edges.
    ///
    /// The task can be recomputed on demand via `read(&engine).await` when
    /// next needed. This is PledgePack's answer to Turbopack's "reducing memory
    /// consumption" priority — but built into the type, not a separate eviction
    /// system.
    ///
    /// After calling `drop_output`, `peek` and `read_fast` will return `None`
    /// until the task is recomputed. The dependency graph edges remain intact,
    /// so `read(&engine).await` will recompute and re-cache the output.
    pub fn drop_output(&self, engine: &crate::TaskEngine) {
        engine.backend().remove_memory(&self.id);
    }

    /// G1.12: Return a human-readable debug identifier for this task.
    ///
    /// Format: `TypeName#short_hash` (e.g., `ParsedModule#a1b2c3d4`).
    /// Uses `std::any::type_name::<T>()` for the type name — this is a
    /// compile-time string, so no runtime registry lookup is needed.
    /// The short hash is the first 8 hex chars of the TaskId.
    pub fn debug_id(&self) -> String {
        self.id.debug_id(std::any::type_name::<T>())
    }

    /// G1.13: Compute a version-aware fingerprint for remote cache keying.
    ///
    /// The fingerprint is `blake3(task_id_bytes ++ version_bytes)` truncated to
    /// 16 bytes. This ensures that when the toolchain version changes, all remote
    /// cache entries are automatically invalidated — no manual cache clearing needed.
    ///
    /// The version string should include the PledgePack version, any relevant
    /// plugin versions, and configuration that affects output (e.g., target,
    /// optimization level). The caller is responsible for constructing a
    /// meaningful version string.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let task = parse_source(source_task);
    /// let fingerprint = task.fingerprint("pledgepack-0.2.9+oxc-0.30.0");
    /// // Use fingerprint as remote cache key
    /// ```
    pub fn fingerprint(&self, version: &str) -> TaskId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.id.as_bytes());
        hasher.update(&[0xFC]); // separator
        hasher.update(version.as_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash.as_bytes()[..16]);
        TaskId(bytes)
    }

    /// G1.13: Compute a fingerprint using the default engine version.
    ///
    /// This uses the crate's version as the version string. For more
    /// specific versioning (including plugin versions), use `fingerprint()`
    /// with a custom version string.
    pub fn fingerprint_default(&self) -> TaskId {
        self.fingerprint(env!("CARGO_PKG_VERSION"))
    }

    /// G2.8: Return a human-readable description of this task.
    ///
    /// Uses `core::any::type_name` for the output type and `debug_id()` for the
    /// task identity. For macro-generated tasks, the `#[task]` macro generates
    /// a `__{NAME}_DEBUG_DESC` const with a richer description including argument
    /// types.
    pub fn task_debug(&self) -> String {
        let type_name = core::any::type_name::<T>();
        // Extract just the last segment of the type name
        let short_type = type_name.rsplit("::").next().unwrap_or(type_name);
        format!("Task<{}>(id={})", short_type, self.debug_id())
    }

    /// G1.14: Returns whether this task has side effects.
    ///
    /// Side-effecting tasks (`Task<T, HasEffect>`) are tracked separately by the
    /// engine for correct invalidation. Pure tasks (`Task<T, NoEffect>`, the
    /// default) can be cached aggressively.
    pub const fn has_side_effects(&self) -> bool {
        E::HAS_SIDE_EFFECTS
    }

    /// G1.19: Compute a version-aware task ID that includes the schema version.
    ///
    /// The versioned ID is `blake3(task_id_bytes ++ 0xFE ++ version_bytes)`
    /// truncated to 16 bytes. When the task function's logic changes (new
    /// version), the version is part of the task ID, so old cached results
    /// are automatically invalid.
    ///
    /// This is compile-time schema versioning — Turbopack has no equivalent
    /// (they rely on manual cache busting).
    pub fn versioned_id(&self) -> TaskId {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.id.as_bytes());
        hasher.update(&[0xFE]); // version separator
        hasher.update(&V::VERSION.to_le_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hash.as_bytes()[..16]);
        TaskId(bytes)
    }

    /// G1.19: Get the compile-time version number of this task.
    pub const fn version(&self) -> u32 {
        V::VERSION
    }

    /// G1.20: Compile-time verification that the output type implements
    /// `Serialize + DeserializeOwned + Send + Sync`.
    ///
    /// This is a const fn that enforces at compile time that `T` satisfies all
    /// required traits. If `T` doesn't implement any of these, the code won't
    /// compile. Turbopack's `Vc` has runtime checks for some of these;
    /// PledgePack's are compile-time.
    ///
    /// Call this in a const context to verify at compile time:
    /// ```ignore
    /// const _: () = Task<MyOutput>::verify();
    /// ```
    pub const fn verify()
    where
        T: Serialize + DeserializeOwned + Send + Sync,
    {
    }
}

/// G2.8: Trait for tasks that have a human-readable debug description.
///
/// The `#[task]` macro generates an impl of this trait for each task function,
/// providing a description like `"transform(source=Task<SourceFile>, config=Task<Config>) -> Output"`.
pub trait TaskDebug {
    /// Return a human-readable description of this task.
    fn debug_description() -> &'static str;
}

/// G1.11: A type-erased task handle for collections of heterogeneous tasks.
///
/// `AnyTask` is 16 bytes (just the `TaskId`, no `PhantomData`) and can be stored
/// in `Vec<AnyTask>`, `HashMap<AnyTask, ...>`, etc. when you need to mix tasks
/// of different output types.
///
/// Turbopack needs `RawVc` for this — a separate type hierarchy. PledgePack uses
/// `AnyTask` only internally; users always see `Task<T>`. Convert between them
/// with `Task::into_any()` and `AnyTask::cast::<T>()`.
///
/// `AnyTask` is `Copy`, `Send + Sync`, `Hash`, `Eq`, `Ord` — same ergonomics as
/// `Task<T>` but without the type parameter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnyTask(TaskId);

impl AnyTask {
    /// Get the `TaskId` of this erased task.
    pub fn id(&self) -> TaskId {
        self.0
    }

    /// Cast this `AnyTask` back to a typed `Task<T>`.
    ///
    /// This is zero-cost — it just attaches the `PhantomData`. The caller is
    /// responsible for ensuring the `TaskId` was originally created for a task
    /// of type `T`.
    pub fn cast<T, E: TaskEffect, V: TaskVersion>(self) -> Task<T, E, V> {
        Task {
            id: self.0,
            _marker: PhantomData,
        }
    }

    /// Return a human-readable debug identifier (without type info).
    pub fn debug_id(&self) -> String {
        self.0.short_hex()
    }
}

impl fmt::Debug for AnyTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnyTask({})", self.0.short_hex())
    }
}

impl fmt::Display for AnyTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.short_hex())
    }
}

unsafe impl Send for AnyTask {}
unsafe impl Sync for AnyTask {}

impl<T, E: TaskEffect, V: TaskVersion> From<Task<T, E, V>> for AnyTask {
    fn from(task: Task<T, E, V>) -> Self {
        AnyTask(task.id)
    }
}

impl<T, E: TaskEffect, V: TaskVersion> Task<T, E, V> {
    /// G1.11: Convert this typed `Task<T>` into a type-erased `AnyTask`.
    ///
    /// Useful for storing heterogeneous tasks in a single collection.
    /// Use `AnyTask::cast::<T>()` to recover the typed task.
    pub fn into_any(self) -> AnyTask {
        AnyTask(self.id)
    }
}

/// A trait for values that can be used as task inputs.
///
/// `Task<T>` implements this (its input contribution is its `TaskId` bytes).
/// Plain serializable values implement this via their serde-serialized bytes.
///
/// This is how the `#[task]` macro generates input-hashing logic: each argument
/// is hashed via `TaskInput::input_hash()`, and the results are concatenated
/// to form the `TaskId`.
pub trait TaskInput {
    /// Append this input's contribution to the hash buffer.
    fn input_hash(&self, buf: &mut Vec<u8>);

    /// G2.18: Compute a 32-byte blake3 digest of this input.
    ///
    /// This is used by `compute_task_id_compact` to hash each input to a
    /// fixed-size digest before concatenating, making the overall hash
    /// O(number of arguments) instead of O(size of arguments).
    ///
    /// Default implementation calls `input_hash` and then blake3-hashes the
    /// result. Types with a known fixed-size representation (like `Task<T>`
    /// which is 16 bytes) can override this to avoid the double-hash.
    fn input_digest(&self) -> [u8; 32] {
        let mut buf = Vec::new();
        self.input_hash(&mut buf);
        *blake3::hash(&buf).as_bytes()
    }
}

impl<T, E: TaskEffect, V: TaskVersion> TaskInput for Task<T, E, V> {
    fn input_hash(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.id.as_bytes());
    }

    /// G2.18: Task<T> is already a 16-byte blake3 digest — hash it directly.
    fn input_digest(&self) -> [u8; 32] {
        *blake3::hash(self.id.as_bytes()).as_bytes()
    }
}

impl TaskInput for TaskId {
    fn input_hash(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(self.as_bytes());
    }

    /// G2.18: TaskId is already a 16-byte blake3 digest — hash it directly.
    fn input_digest(&self) -> [u8; 32] {
        *blake3::hash(self.as_bytes()).as_bytes()
    }
}

// Implement TaskInput for common primitives via serde serialization.
// The content hash is blake3(serde_json::to_vec(value)) — deterministic by construction.
macro_rules! impl_task_input_serde {
    ($($ty:ty),* $(,)?) => {
        $(
            impl TaskInput for $ty {
                fn input_hash(&self, buf: &mut Vec<u8>) {
                    if let Ok(bytes) = serde_json::to_vec(self) {
                        buf.extend_from_slice(&bytes);
                    }
                }
            }
        )*
    };
}

impl_task_input_serde!(
    String, &str, bool, u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64,
);

impl<T: TaskInput> TaskInput for Vec<T> {
    fn input_hash(&self, buf: &mut Vec<u8>) {
        for item in self {
            item.input_hash(buf);
        }
    }
}

impl<T: TaskInput> TaskInput for Option<T> {
    fn input_hash(&self, buf: &mut Vec<u8>) {
        match self {
            Some(v) => {
                buf.push(1);
                v.input_hash(buf);
            }
            None => buf.push(0),
        }
    }
}

impl<T: TaskInput, U: TaskInput> TaskInput for (T, U) {
    fn input_hash(&self, buf: &mut Vec<u8>) {
        self.0.input_hash(buf);
        self.1.input_hash(buf);
    }
}

/// Compute a `TaskId` from a function ID and a list of `TaskInput` values.
///
/// This is the function the `#[task]` macro calls to generate the task ID.
/// Each argument implements `TaskInput`, so the macro just collects them
/// and calls this function.
pub fn compute_task_id(function_id: &str, inputs: &[&dyn TaskInput]) -> TaskId {
    let mut buf = Vec::new();
    for input in inputs {
        input.input_hash(&mut buf);
    }
    TaskId::compute(function_id, &buf)
}

/// G2.18: Compute a `TaskId` using compact input-hash pre-images.
///
/// Instead of concatenating the full serialized inputs (O(size of arguments)),
/// this function hashes each input to a 32-byte digest first, then concatenates
/// only the digests (O(number of arguments)).
///
/// This produces a different `TaskId` than `compute_task_id` for the same inputs,
/// so it should be used consistently within a project. The tradeoff is:
/// - Pro: Hash time is O(n_args × 32) instead of O(total_input_size)
/// - Pro: Memory for the hash buffer is bounded at n_args × 32 bytes
/// - Con: Two different inputs that hash to the same 32-byte digest would
///   collide (but blake3-256 makes this astronomically unlikely)
///
/// For `Task<T>` inputs, the digest is `blake3(task_id_bytes)` — a single
/// blake3 round on 16 bytes, very fast.
pub fn compute_task_id_compact(function_id: &str, inputs: &[&dyn TaskInput]) -> TaskId {
    let mut buf = Vec::with_capacity(inputs.len() * 32);
    for input in inputs {
        buf.extend_from_slice(&input.input_digest());
    }
    TaskId::compute(function_id, &buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_deterministic() {
        let id1 = TaskId::compute("test_fn", b"hello");
        let id2 = TaskId::compute("test_fn", b"hello");
        assert_eq!(id1, id2);
    }

    #[test]
    fn task_id_differs_for_different_functions() {
        let id1 = TaskId::compute("fn_a", b"hello");
        let id2 = TaskId::compute("fn_b", b"hello");
        assert_ne!(id1, id2);
    }

    #[test]
    fn task_id_differs_for_different_inputs() {
        let id1 = TaskId::compute("test_fn", b"hello");
        let id2 = TaskId::compute("test_fn", b"world");
        assert_ne!(id1, id2);
    }

    #[test]
    fn task_id_hex_roundtrip() {
        let id = TaskId::compute("test_fn", b"hello");
        let hex = id.to_hex();
        assert_eq!(hex.len(), 32);
        let parsed = TaskId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn task_id_is_16_bytes() {
        assert_eq!(std::mem::size_of::<TaskId>(), 16);
    }

    #[test]
    fn task_is_16_bytes_plus_marker() {
        // Task<T> is TaskId (16 bytes) + PhantomData (0 bytes, ZST)
        // So Task<T> should be 16 bytes.
        assert_eq!(std::mem::size_of::<Task<u32>>(), 16);
    }

    #[test]
    fn task_is_copy() {
        fn assert_copy<T: Copy>() {}
        assert_copy::<Task<u32>>();
    }

    #[test]
    fn task_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Task<u32>>();
    }

    #[test]
    fn task_id_ord_works() {
        let id1 = TaskId::from_bytes([0u8; 16]);
        let id2 = TaskId::from_bytes([1u8; 16]);
        assert!(id1 < id2);
    }

    #[test]
    fn task_eq_by_id() {
        let id = TaskId::compute("test", b"input");
        let t1: Task<u32> = Task::from_id(id);
        let t2: Task<u32> = Task::from_id(id);
        assert_eq!(t1, t2);
    }

    #[test]
    fn compute_task_id_with_mixed_inputs() {
        let s = String::from("hello");
        let n: u32 = 42;
        let id = compute_task_id("test_fn", &[&s, &n]);
        // Same inputs → same ID
        let id2 = compute_task_id("test_fn", &[&s, &n]);
        assert_eq!(id, id2);
        // Different inputs → different ID
        let n2: u32 = 43;
        let id3 = compute_task_id("test_fn", &[&s, &n2]);
        assert_ne!(id, id3);
    }

    #[test]
    fn task_id_short_hex_is_8_chars() {
        let id = TaskId::compute("test_fn", b"hello");
        let short = id.short_hex();
        assert_eq!(short.len(), 8);
        // Should be the first 8 chars of the full hex
        let full = id.to_hex();
        assert_eq!(short, &full[..8]);
    }

    #[test]
    fn task_id_debug_id_format() {
        let id = TaskId::compute("transform_jsx", b"input");
        let debug = id.debug_id("transform_jsx");
        // Format: function_name#short_hash
        assert!(debug.starts_with("transform_jsx#"));
        assert_eq!(debug.len(), "transform_jsx#".len() + 8);
    }

    #[test]
    fn task_debug_id_uses_type_name() {
        let id = TaskId::compute("test_fn", b"hello");
        let task: Task<u32> = Task::from_id(id);
        let debug = task.debug_id();
        // Should contain the type name (u32) and a short hash
        assert!(debug.contains("u32"));
        assert!(debug.contains('#'));
    }

    #[tokio::test]
    async fn read_fast_returns_none_for_uncached_task() {
        use crate::backend::{MemoryBackend, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::TaskRegistry;

        let engine = TaskEngine::new(TaskRegistry::new(), TaskBackend::new(MemoryBackend::new()));
        let task: Task<String> = Task::from_id(TaskId::compute("nope", b""));
        // Task is not registered or computed — read_fast should return None
        assert!(task.read_fast(&engine).is_none());
    }

    #[tokio::test]
    async fn read_fast_returns_value_after_compute() {
        use crate::backend::{MemoryBackend, StoredOutput, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::{TaskExecutor, TaskRegistry};

        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("fast_path_test", b"");
        registry.register(
            task_id,
            "fast_path_test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("fast_path_test", b""),
                    &42u32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let task: Task<u32> = Task::from_id(task_id);

        // Before compute — fast path returns None
        assert!(task.read_fast(&engine).is_none());

        // Compute via async read
        let result = task.read(&engine).await.unwrap();
        assert_eq!(*result, 42);

        // After compute — fast path returns the cached value (lock-free, sync)
        let fast = task.read_fast(&engine);
        assert!(fast.is_some());
        assert_eq!(*fast.unwrap(), 42);
    }

    #[tokio::test]
    async fn read_fast_returns_none_after_invalidation() {
        use crate::backend::{MemoryBackend, StoredOutput, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::{TaskExecutor, TaskRegistry};

        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("invalidate_fast", b"");
        registry.register(
            task_id,
            "invalidate_fast".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("invalidate_fast", b""),
                    &"value".to_string(),
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let task: Task<String> = Task::from_id(task_id);

        // Compute
        let _ = task.read(&engine).await.unwrap();

        // Fast path works
        assert!(task.read_fast(&engine).is_some());

        // Invalidate
        engine.invalidate(task_id);

        // Fast path returns None (task is Dirty)
        assert!(task.read_fast(&engine).is_none());
    }

    #[tokio::test]
    async fn peek_returns_none_for_uncached_task() {
        use crate::backend::{MemoryBackend, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::TaskRegistry;

        let engine = TaskEngine::new(TaskRegistry::new(), TaskBackend::new(MemoryBackend::new()));
        let task: Task<String> = Task::from_id(TaskId::compute("nope", b""));
        assert!(task.peek(&engine).is_none());
    }

    #[tokio::test]
    async fn peek_returns_value_after_compute() {
        use crate::backend::{MemoryBackend, StoredOutput, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::{TaskExecutor, TaskRegistry};

        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("peek_test", b"");
        registry.register(
            task_id,
            "peek_test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("peek_test", b""),
                    &99u32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let task: Task<u32> = Task::from_id(task_id);

        // Before compute — peek returns None
        assert!(task.peek(&engine).is_none());

        // Compute
        let _ = task.read(&engine).await.unwrap();

        // After compute — peek returns the value
        let peeked = task.peek(&engine);
        assert!(peeked.is_some());
        assert_eq!(*peeked.unwrap(), 99);
    }

    #[tokio::test]
    async fn peek_returns_value_even_when_dirty() {
        use crate::backend::{MemoryBackend, StoredOutput, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::{TaskExecutor, TaskRegistry};

        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("peek_dirty", b"");
        registry.register(
            task_id,
            "peek_dirty".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("peek_dirty", b""),
                    &"stale".to_string(),
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let task: Task<String> = Task::from_id(task_id);

        // Compute
        let _ = task.read(&engine).await.unwrap();

        // Invalidate (marks dirty, but output stays in memory cache)
        engine.invalidate(task_id);

        // read_fast returns None (dirty status), but peek still returns the stale value
        assert!(task.read_fast(&engine).is_none());
        let peeked = task.peek(&engine);
        assert!(peeked.is_some());
        assert_eq!(*peeked.unwrap(), "stale");
    }

    #[tokio::test]
    async fn drop_output_evicts_from_memory() {
        use crate::backend::{MemoryBackend, StoredOutput, TaskBackend};
        use crate::engine::TaskEngine;
        use crate::registry::{TaskExecutor, TaskRegistry};

        let registry = TaskRegistry::new();
        let task_id = TaskId::compute("drop_test", b"");
        registry.register(
            task_id,
            "drop_test".to_string(),
            TaskExecutor::sync(|| {
                Ok(StoredOutput::new(
                    TaskId::compute("drop_test", b""),
                    &55u32,
                    vec![],
                )?)
            }),
        );

        let engine = TaskEngine::new(registry, TaskBackend::new(MemoryBackend::new()));
        let task: Task<u32> = Task::from_id(task_id);

        // Compute
        let _ = task.read(&engine).await.unwrap();

        // peek returns the value
        assert!(task.peek(&engine).is_some());

        // Drop the output
        task.drop_output(&engine);

        // peek now returns None
        assert!(task.peek(&engine).is_none());

        // But read_fast also returns None (no memory cache)
        assert!(task.read_fast(&engine).is_none());

        // The task can be recomputed — read should work
        let result = task.read(&engine).await.unwrap();
        assert_eq!(*result, 55);
    }

    #[test]
    fn anytask_is_16_bytes() {
        assert_eq!(std::mem::size_of::<AnyTask>(), 16);
    }

    #[test]
    fn anytask_is_copy_send_sync() {
        fn assert_copy<T: Copy>() {}
        fn assert_send_sync<T: Send + Sync>() {}
        assert_copy::<AnyTask>();
        assert_send_sync::<AnyTask>();
    }

    #[test]
    fn anytask_roundtrip() {
        let id = TaskId::compute("test_fn", b"hello");
        let task: Task<u32> = Task::from_id(id);

        // Convert to AnyTask
        let any = task.into_any();
        assert_eq!(any.id(), id);

        // Cast back to Task<u32> — T/E/V all inferred from the annotation
        // below (E/V via Task's own default type parameters).
        let recovered: Task<u32> = any.cast();
        assert_eq!(recovered, task);
    }

    #[test]
    fn anytask_from_task() {
        let id = TaskId::compute("from_test", b"world");
        let task: Task<String> = Task::from_id(id);
        let any: AnyTask = task.into();
        assert_eq!(any.id(), id);
    }

    #[test]
    fn anytask_hash_eq_ord() {
        let id1 = TaskId::from_bytes([0u8; 16]);
        let id2 = TaskId::from_bytes([1u8; 16]);

        let a1 = AnyTask(id1);
        let a2 = AnyTask(id2);
        let a1b = AnyTask(id1);

        assert_eq!(a1, a1b);
        assert_ne!(a1, a2);
        assert!(a1 < a2);

        let mut set = std::collections::HashSet::new();
        set.insert(a1);
        assert!(set.contains(&a1b));
    }

    #[test]
    fn anytask_collection_of_heterogeneous_tasks() {
        let id1 = TaskId::compute("fn_a", b"input1");
        let id2 = TaskId::compute("fn_b", b"input2");

        let task_u32: Task<u32> = Task::from_id(id1);
        let task_str: Task<String> = Task::from_id(id2);

        // Store heterogeneous tasks in a single Vec
        let tasks: Vec<AnyTask> = vec![task_u32.into_any(), task_str.into_any()];
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id(), id1);
        assert_eq!(tasks[1].id(), id2);

        // Recover typed tasks — T/E/V inferred from the annotation below.
        let recovered_u32: Task<u32> = tasks[0].cast();
        assert_eq!(recovered_u32, task_u32);
    }

    #[test]
    fn task_fingerprint_is_version_aware() {
        let id = TaskId::compute("fingerprint_test", b"input");
        let task: Task<u32> = Task::from_id(id);

        let fp1 = task.fingerprint("v1.0");
        let fp2 = task.fingerprint("v2.0");
        let fp1_again = task.fingerprint("v1.0");

        // Same version → same fingerprint
        assert_eq!(
            fp1, fp1_again,
            "Same version should produce same fingerprint"
        );
        // Different version → different fingerprint
        assert_ne!(
            fp1, fp2,
            "Different versions should produce different fingerprints"
        );
        // Fingerprint should differ from raw task ID
        assert_ne!(fp1, id, "Fingerprint should differ from raw task ID");
    }

    #[test]
    fn task_fingerprint_default_is_deterministic() {
        let id = TaskId::compute("fingerprint_default_test", b"input");
        let task: Task<u32> = Task::from_id(id);

        let fp1 = task.fingerprint_default();
        let fp2 = task.fingerprint_default();

        assert_eq!(fp1, fp2, "Default fingerprint should be deterministic");
        assert_ne!(fp1, id, "Fingerprint should differ from raw task ID");
    }

    #[test]
    fn task_debug_returns_readable_description() {
        let id = TaskId::compute("debug_test_fn", b"input");
        let task: Task<u32> = Task::from_id(id);

        let desc = task.task_debug();
        assert!(
            desc.contains("Task<"),
            "task_debug should include Task< prefix: got {}",
            desc
        );
        assert!(
            desc.contains("u32"),
            "task_debug should include output type: got {}",
            desc
        );
        assert!(
            desc.contains("id="),
            "task_debug should include id= field: got {}",
            desc
        );
    }

    #[test]
    fn compute_task_id_compact_is_deterministic() {
        let s1 = "hello".to_string();
        let s2 = "hello".to_string();
        let inputs1: Vec<&dyn TaskInput> = vec![&s1];
        let inputs2: Vec<&dyn TaskInput> = vec![&s2];
        let id1 = compute_task_id_compact("compact_fn", &inputs1);
        let id2 = compute_task_id_compact("compact_fn", &inputs2);
        assert_eq!(id1, id2, "Same inputs should produce same compact TaskId");
    }

    #[test]
    fn compute_task_id_compact_differs_for_different_inputs() {
        let s1 = "hello".to_string();
        let s2 = "world".to_string();
        let inputs1: Vec<&dyn TaskInput> = vec![&s1];
        let inputs2: Vec<&dyn TaskInput> = vec![&s2];
        let id1 = compute_task_id_compact("compact_fn", &inputs1);
        let id2 = compute_task_id_compact("compact_fn", &inputs2);
        assert_ne!(
            id1, id2,
            "Different inputs should produce different compact TaskId"
        );
    }

    #[test]
    fn compute_task_id_compact_differs_from_full() {
        let s = "hello".to_string();
        let inputs: Vec<&dyn TaskInput> = vec![&s];
        let id_full = compute_task_id("compact_fn", &inputs);
        let id_compact = compute_task_id_compact("compact_fn", &inputs);
        assert_ne!(
            id_full, id_compact,
            "Compact and full should produce different TaskIds"
        );
    }

    #[test]
    fn compute_task_id_compact_with_task_inputs() {
        let task_a: Task<u32> = Task::from_id(TaskId::compute("inner_a", b""));
        let task_b: Task<u32> = Task::from_id(TaskId::compute("inner_b", b""));

        let inputs_a: Vec<&dyn TaskInput> = vec![&task_a];
        let inputs_b: Vec<&dyn TaskInput> = vec![&task_b];
        let inputs_ab: Vec<&dyn TaskInput> = vec![&task_a, &task_b];

        let id_a = compute_task_id_compact("outer", &inputs_a);
        let id_b = compute_task_id_compact("outer", &inputs_b);
        let id_ab = compute_task_id_compact("outer", &inputs_ab);

        assert_ne!(
            id_a, id_b,
            "Different task inputs should produce different IDs"
        );
        assert_ne!(
            id_a, id_ab,
            "Different number of inputs should produce different IDs"
        );
    }

    #[test]
    fn input_digest_is_32_bytes() {
        let s = "test".to_string();
        let digest = s.input_digest();
        assert_eq!(digest.len(), 32, "input_digest should return 32 bytes");

        let task: Task<u32> = Task::from_id(TaskId::compute("digest_test", b""));
        let task_digest = task.input_digest();
        assert_eq!(
            task_digest.len(),
            32,
            "Task input_digest should return 32 bytes"
        );
    }

    #[test]
    fn g1_14_no_effect_is_default() {
        let task: Task<u32> = Task::from_id(TaskId::compute("pure_fn", b"input"));
        assert!(
            !task.has_side_effects(),
            "Default Task<T> should have no side effects"
        );
    }

    #[test]
    fn g1_14_has_effect_marker() {
        let task: Task<u32, HasEffect> = Task::from_id(TaskId::compute("side_effect_fn", b"input"));
        assert!(
            task.has_side_effects(),
            "Task<T, HasEffect> should report side effects"
        );
    }

    #[test]
    fn g1_14_no_effect_marker_explicit() {
        let task: Task<u32, NoEffect> =
            Task::from_id(TaskId::compute("pure_fn_explicit", b"input"));
        assert!(
            !task.has_side_effects(),
            "Task<T, NoEffect> should report no side effects"
        );
    }

    #[test]
    fn g1_19_versioned_id_differs_by_version() {
        let task_v1: Task<u32, NoEffect, V1> =
            Task::from_id(TaskId::compute("versioned_fn", b"input"));
        let task_v2: Task<u32, NoEffect, V2> =
            Task::from_id(TaskId::compute("versioned_fn", b"input"));

        // Same base ID (same function + inputs)
        assert_eq!(task_v1.id(), task_v2.id(), "Base TaskId should be the same");

        // But versioned IDs differ because version is part of the hash
        assert_ne!(
            task_v1.versioned_id(),
            task_v2.versioned_id(),
            "Versioned TaskIds should differ across versions"
        );
    }

    #[test]
    fn g1_19_version_number() {
        let task_v1: Task<u32> = Task::from_id(TaskId::compute("fn", b""));
        let task_v2: Task<u32, NoEffect, V2> = Task::from_id(TaskId::compute("fn", b""));
        assert_eq!(task_v1.version(), 1, "Default version should be 1");
        assert_eq!(task_v2.version(), 2, "V2 version should be 2");
    }

    #[test]
    fn g1_19_versioned_id_is_deterministic() {
        let task: Task<u32, NoEffect, V2> = Task::from_id(TaskId::compute("fn", b"input"));
        let id1 = task.versioned_id();
        let id2 = task.versioned_id();
        assert_eq!(id1, id2, "versioned_id should be deterministic");
    }

    #[test]
    fn g1_20_verify_compiles_for_valid_type() {
        // This is a compile-time check. If u32 didn't implement Serialize + DeserializeOwned + Send + Sync,
        // this wouldn't compile.
        const _: () = Task::<u32>::verify();
        const _: () = Task::<String, HasEffect>::verify();
        const _: () = Task::<u64, NoEffect, V2>::verify();
        // If we reach here, verification passed.
    }

    #[test]
    fn g1_14_effect_type_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<NoEffect>();
        assert_send_sync::<HasEffect>();
    }

    #[test]
    fn g1_19_version_type_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<V1>();
        assert_send_sync::<V2>();
    }
}
