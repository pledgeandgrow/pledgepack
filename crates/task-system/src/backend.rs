// Task backend — storage for task outputs.
//
// Three-tier storage (same architecture as the existing cache, but for task outputs):
//   1. Memory: DashMap<TaskId, Arc<serialized output bytes>>
//   2. Disk: bincode-serialized, mmap for large entries, atomic writes
//   3. Remote: HTTP/S3/GCS via pledgepack-cache's RemoteCache (existing backends)
//
// The task ID IS the cache key — no separate metadata. Fetch by hash, store by hash.
// This integrates with the existing pledgepack-cache crate for disk + remote backends.

use crate::task::TaskId;
use dashmap::DashMap;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::{debug, trace};

/// A serialized task output stored in the backend.
///
/// Outputs are stored as `serde_json` bytes (deterministic, debuggable) with a
/// content hash for integrity verification. The `output_hash` allows the engine
/// to detect when a recomputed task produces the same output as before — in which
/// case dependents are not invalidated (the content didn't change).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredOutput {
    /// The task ID this output belongs to.
    pub task_id: TaskId,
    /// The serialized output bytes (serde_json).
    pub data: Vec<u8>,
    /// blake3 hash of `data` — used for content-change detection.
    pub output_hash: [u8; 16],
    /// Task IDs that this task depends on (its inputs).
    pub dependencies: Vec<TaskId>,
    /// Whether this task had side effects (non-cacheable output).
    pub has_side_effects: bool,
    /// G2.11: Unix timestamp when this output expires (0 = no TTL / never expires).
    #[serde(default)]
    pub expires_at: u64,
    /// File paths read during task execution (read-tracked dependencies).
    /// Supplements `dependencies` (explicit Task<T> deps) with implicit file deps.
    /// When any of these files change, the task is invalidated.
    #[serde(default)]
    pub read_dependencies: Vec<String>,
}

impl StoredOutput {
    /// Serialize a value into a `StoredOutput`.
    pub fn new<T: Serialize>(task_id: TaskId, value: &T, dependencies: Vec<TaskId>) -> Result<Self, serde_json::Error> {
        let data = serde_json::to_vec(value)?;
        let output_hash = blake3::hash(&data).as_bytes()[..16].try_into().unwrap();
        Ok(StoredOutput {
            task_id,
            data,
            output_hash,
            dependencies,
            has_side_effects: false,
            expires_at: 0,
            read_dependencies: Vec::new(),
        })
    }

    /// Serialize a value into a `StoredOutput` with read-tracked file dependencies.
    pub fn new_with_reads<T: Serialize>(
        task_id: TaskId,
        value: &T,
        dependencies: Vec<TaskId>,
        read_deps: Vec<String>,
    ) -> Result<Self, serde_json::Error> {
        let data = serde_json::to_vec(value)?;
        let output_hash = blake3::hash(&data).as_bytes()[..16].try_into().unwrap();
        Ok(StoredOutput {
            task_id,
            data,
            output_hash,
            dependencies,
            has_side_effects: false,
            expires_at: 0,
            read_dependencies: read_deps,
        })
    }

    /// Serialize a value into a non-cacheable `StoredOutput` (G2.10).
    ///
    /// The output is marked with `has_side_effects: true`, which tells the
    /// engine to skip caching (memory, disk, remote) for this task.
    pub fn new_non_cacheable<T: Serialize>(
        task_id: TaskId,
        value: &T,
        dependencies: Vec<TaskId>,
    ) -> Result<Self, serde_json::Error> {
        let data = serde_json::to_vec(value)?;
        let output_hash = blake3::hash(&data).as_bytes()[..16].try_into().unwrap();
        Ok(StoredOutput {
            task_id,
            data,
            output_hash,
            dependencies,
            has_side_effects: true,
            expires_at: 0,
            read_dependencies: Vec::new(),
        })
    }

    /// Serialize a value into a `StoredOutput` with a TTL (G2.11).
    ///
    /// The output will expire after `ttl_secs` seconds from now.
    /// When expired, the engine treats it as a cache miss and recomputes.
    pub fn new_with_ttl<T: Serialize>(
        task_id: TaskId,
        value: &T,
        dependencies: Vec<TaskId>,
        ttl_secs: u64,
    ) -> Result<Self, serde_json::Error> {
        let data = serde_json::to_vec(value)?;
        let output_hash = blake3::hash(&data).as_bytes()[..16].try_into().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Ok(StoredOutput {
            task_id,
            data,
            output_hash,
            dependencies,
            has_side_effects: false,
            expires_at: if ttl_secs > 0 { now + ttl_secs } else { 0 },
            read_dependencies: Vec::new(),
        })
    }

    /// G2.11: Check if this output has expired.
    ///
    /// Returns `true` if `expires_at > 0` and the current time is past `expires_at`.
    /// Returns `false` if `expires_at == 0` (no TTL) or not yet expired.
    pub fn is_expired(&self) -> bool {
        if self.expires_at == 0 {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now >= self.expires_at
    }

    /// Deserialize the output value from the stored bytes.
    pub fn deserialize<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.data)
    }
}

/// In-memory task output storage.
///
/// This is the hot path — O(1) DashMap lookup by TaskId.
/// Thread-safe via DashMap's sharded locks.
///
/// G4.7: Includes LRU tracking for memory-pressure eviction.
#[derive(Default)]
pub struct MemoryBackend {
    outputs: DashMap<TaskId, Arc<StoredOutput>>,
    /// Track output hashes for content-change detection.
    /// When a task is recomputed and produces the same output_hash,
    /// dependents are not invalidated.
    output_hashes: DashMap<TaskId, [u8; 16]>,
    /// G4.7: LRU access tracking — task ID → last access timestamp (monotonic counter).
    lru: Mutex<HashMap<TaskId, u64>>,
    /// G4.7: Monotonic counter for LRU ordering.
    lru_counter: std::sync::atomic::AtomicU64,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a task output. Overwrites if the task ID already exists.
    pub fn store(&self, output: StoredOutput) {
        let id = output.task_id;
        let hash = output.output_hash;
        self.outputs.insert(id, Arc::new(output));
        self.output_hashes.insert(id, hash);
        self.touch_lru(id);
    }

    /// Get a task output. Returns `Arc<StoredOutput>` — cheap clone.
    pub fn get(&self, id: &TaskId) -> Option<Arc<StoredOutput>> {
        let result = self.outputs.get(id).map(|r| Arc::clone(&r));
        if result.is_some() {
            self.touch_lru(*id);
        }
        result
    }

    /// Check if a task is cached.
    pub fn contains(&self, id: &TaskId) -> bool {
        self.outputs.contains_key(id)
    }

    /// Get the output hash for a task (for content-change detection).
    pub fn output_hash(&self, id: &TaskId) -> Option<[u8; 16]> {
        self.output_hashes.get(id).map(|r| *r)
    }

    /// Remove a task from the cache.
    ///
    /// All `self.lru.lock()` calls in this file use `.unwrap_or_else(|e|
    /// e.into_inner())` rather than `.unwrap()`: a bare `.unwrap()` panics
    /// the *next* caller too if any earlier holder of this same lock ever
    /// panicked while holding it, turning one unrelated bug into a
    /// permanently wedged build engine. `PoisonError::into_inner()` recovers
    /// the (possibly-inconsistent-but-still-usable) guard instead. See
    /// PRODUCTION-READINESS-100.md goal 21.
    pub fn remove(&self, id: &TaskId) {
        self.outputs.remove(id);
        self.output_hashes.remove(id);
        self.lru.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    /// G4.7: Evict the least recently accessed clean output.
    ///
    /// Returns the evicted TaskId, or None if the cache is empty.
    pub fn evict_lru(&self) -> Option<TaskId> {
        let lru = self.lru.lock().unwrap_or_else(|e| e.into_inner());
        if lru.is_empty() {
            return None;
        }
        // Find the task with the smallest (oldest) access timestamp
        let evict_id = lru.iter().min_by_key(|(_, ts)| *ts).map(|(id, _)| *id)?;
        drop(lru);
        self.remove(&evict_id);
        Some(evict_id)
    }

    /// G4.7: Evict outputs until the cache has at most `max_entries` items.
    ///
    /// Returns the number of evicted entries.
    pub fn evict_to_max(&self, max_entries: usize) -> usize {
        let mut evicted = 0;
        while self.outputs.len() > max_entries {
            if self.evict_lru().is_none() {
                break;
            }
            evicted += 1;
        }
        evicted
    }

    /// G4.7: Touch the LRU counter for a task.
    fn touch_lru(&self, id: TaskId) {
        let ts = self.lru_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        self.lru.lock().unwrap_or_else(|e| e.into_inner()).insert(id, ts);
    }

    /// Number of cached outputs.
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    /// Is the cache empty?
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Clear all cached outputs.
    pub fn clear(&self) {
        self.outputs.clear();
        self.output_hashes.clear();
    }

    /// Get all cached task IDs.
    pub fn ids(&self) -> Vec<TaskId> {
        self.outputs.iter().map(|r| *r.key()).collect()
    }
}

/// Disk-backed task output storage.
///
/// Uses the existing `pledgepack_cache::FunctionCache` for the disk layer,
/// but stores `StoredOutput` (serde_json) instead of the legacy `CachedOutput`.
/// Each task output is stored as `{cache_dir}/tasks/{task_id_hex}.json`.
pub struct DiskBackend {
    cache_dir: PathBuf,
}

impl DiskBackend {
    pub fn new(cache_dir: PathBuf) -> std::io::Result<Self> {
        let tasks_dir = cache_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir)?;
        Ok(DiskBackend { cache_dir: cache_dir.join("tasks") })
    }

    fn path_for(&self, id: &TaskId) -> PathBuf {
        self.cache_dir.join(format!("{}.json", id.to_hex()))
    }

    /// Store a task output to disk (atomic write via temp-file-then-rename).
    pub fn store(&self, output: &StoredOutput) -> std::io::Result<()> {
        let path = self.path_for(&output.task_id);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(output)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &path)?;
        debug!("Stored task output to disk: {}", output.task_id);
        Ok(())
    }

    /// Load a task output from disk.
    ///
    /// G12.37: Verifies content integrity by re-hashing the output data and
    /// comparing to the stored `output_hash`. If the hash doesn't match
    /// (corruption or tampering), the entry is discarded and `None` is returned.
    pub fn get(&self, id: &TaskId) -> std::io::Result<Option<StoredOutput>> {
        let path = self.path_for(id);
        if !path.exists() {
            return Ok(None);
        }
        let data = std::fs::read(&path)?;
        let output: StoredOutput = serde_json::from_slice(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // G12.37: Integrity verification — re-hash the output data and compare
        // to the stored hash. If mismatch, the file was corrupted or tampered.
        let computed_hash: [u8; 16] = blake3::hash(&output.data).as_bytes()[..16]
            .try_into()
            .unwrap();
        if computed_hash != output.output_hash {
            tracing::warn!(
                "Cache integrity check failed for task {}: hash mismatch (expected {:?}, got {:?}), discarding",
                id,
                output.output_hash,
                computed_hash,
            );
            // Remove the corrupted entry
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }

        trace!("Loaded task output from disk: {}", id);
        Ok(Some(output))
    }

    /// Remove a task output from disk.
    pub fn remove(&self, id: &TaskId) -> std::io::Result<()> {
        let path = self.path_for(id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// Clear all task outputs from disk.
    pub fn clear(&self) -> std::io::Result<()> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir)?;
            std::fs::create_dir_all(&self.cache_dir)?;
        }
        Ok(())
    }

    /// List all task IDs on disk.
    pub fn ids(&self) -> std::io::Result<Vec<TaskId>> {
        let mut ids = Vec::new();
        if !self.cache_dir.exists() {
            return Ok(ids);
        }
        for entry in std::fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(hex) = name.strip_suffix(".json") {
                if let Some(id) = TaskId::from_hex(hex) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }
}

/// Three-tier task output storage: memory → disk → remote.
///
/// Lookup order:
///   1. Memory (DashMap, O(1))
///   2. Disk (JSON file, mmap for large)
///   3. Remote (HTTP/S3/GCS via pledgepack-cache)
///
/// On a memory miss, we check disk. On a disk hit, we promote to memory.
/// On a disk miss, we check remote. On a remote hit, we promote to both disk and memory.
/// On a full miss, the task is computed and stored to all tiers.
pub struct TaskBackend {
    pub memory: MemoryBackend,
    disk: Option<DiskBackend>,
    remote: Option<pledgepack_cache::remote::RemoteCache>,
}

impl TaskBackend {
    pub fn new(memory: MemoryBackend) -> Self {
        TaskBackend { memory, disk: None, remote: None }
    }

    pub fn with_disk(mut self, disk: DiskBackend) -> Self {
        self.disk = Some(disk);
        self
    }

    pub fn with_remote(mut self, remote: pledgepack_cache::remote::RemoteCache) -> Self {
        self.remote = Some(remote);
        self
    }

    /// Try to get a task output, checking memory → disk → remote.
    ///
    /// Returns `Some(Arc<StoredOutput>)` if found in any tier, promoting to
    /// higher tiers as needed. Returns `None` if not found anywhere.
    pub fn get(&self, id: &TaskId) -> Option<Arc<StoredOutput>> {
        // 1. Memory
        if let Some(output) = self.memory.get(id) {
            return Some(output);
        }

        // 2. Disk
        if let Some(disk) = &self.disk {
            if let Ok(Some(output)) = disk.get(id) {
                // Promote to memory
                self.memory.store(output.clone());
                return Some(Arc::new(output));
            }
        }

        // 3. Remote — async, so we can't do it here synchronously.
        // The TaskEngine handles remote fetch in an async context.
        None
    }

    /// Synchronous memory-only check (for `try_read`).
    pub fn get_memory(&self, id: &TaskId) -> Option<Arc<StoredOutput>> {
        self.memory.get(id)
    }

    /// Store to memory cache only (for non-cacheable tasks — G2.10).
    pub fn store_memory(&self, output: StoredOutput) {
        self.memory.store(output);
    }

    /// Get the disk backend (if configured).
    pub fn disk(&self) -> Option<&DiskBackend> {
        self.disk.as_ref()
    }

    /// Remove a task output from the memory cache only (for `drop_output`).
    /// Disk and remote caches are not touched — the task can be re-promoted
    /// from disk on next `read`.
    pub fn remove_memory(&self, id: &TaskId) {
        self.memory.remove(id);
    }

    /// Store a task output to all configured tiers.
    pub fn store(&self, output: StoredOutput) {
        // Always store to memory
        self.memory.store(output.clone());

        // Store to disk if configured
        if let Some(disk) = &self.disk {
            if let Err(e) = disk.store(&output) {
                tracing::warn!("Failed to store task output to disk: {}", e);
            }
        }

        // Remote store is async — handled by TaskEngine
    }

    /// Store to remote cache. Called by TaskEngine after computing a task.
    ///
    /// Bridges to the existing `RemoteCache` API by serializing the `StoredOutput`
    /// as JSON into the `RemoteCacheEntry.code` field. When the remote cache is
    /// upgraded to support arbitrary bytes, this can be simplified.
    pub fn store_remote(&self, output: &StoredOutput) -> Result<(), anyhow::Error> {
        if let Some(remote) = &self.remote {
            let key = output.task_id.to_hex();
            let json = serde_json::to_string(output)?;
            let entry = pledgepack_cache::remote::RemoteCacheEntry {
                code: json,
                source_map: None,
                deps: output
                    .dependencies
                    .iter()
                    .map(|d| d.to_hex())
                    .collect(),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            remote.set(&key, &entry)?;
        }
        Ok(())
    }

    /// G1.13: Store to remote cache with a version-aware fingerprint key.
    ///
    /// Uses `fingerprint(task_id, version)` as the remote cache key instead of
    /// the raw task ID. This ensures that when the toolchain version changes,
    /// stale remote cache entries are not fetched.
    pub fn store_remote_versioned(
        &self,
        output: &StoredOutput,
        version: &str,
    ) -> Result<(), anyhow::Error> {
        if let Some(remote) = &self.remote {
            let fingerprint = fingerprint_task_id(&output.task_id, version);
            let key = fingerprint.to_hex();
            let json = serde_json::to_string(output)?;
            let entry = pledgepack_cache::remote::RemoteCacheEntry {
                code: json,
                source_map: None,
                deps: output
                    .dependencies
                    .iter()
                    .map(|d| d.to_hex())
                    .collect(),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            };
            remote.set(&key, &entry)?;
        }
        Ok(())
    }

    /// Fetch from remote cache. Called by TaskEngine on local miss.
    ///
    /// G12.37: Verifies content integrity by re-hashing the output data and
    /// comparing to the stored `output_hash`. If the hash doesn't match
    /// (corruption or tampering), the entry is discarded and `None` is returned.
    pub fn get_remote(&self, id: &TaskId) -> Result<Option<StoredOutput>, anyhow::Error> {
        if let Some(remote) = &self.remote {
            let key = id.to_hex();
            if let Some(entry) = remote.get(&key)? {
                let output: StoredOutput = serde_json::from_str(&entry.code)?;

                // G12.37: Integrity verification — re-hash the output data and
                // compare to the stored hash. Prevents cache poisoning.
                let computed_hash: [u8; 16] = blake3::hash(&output.data).as_bytes()[..16]
                    .try_into()
                    .unwrap();
                if computed_hash != output.output_hash {
                    tracing::warn!(
                        "Remote cache integrity check failed for task {}: hash mismatch, discarding",
                        id,
                    );
                    return Ok(None);
                }

                // Promote to memory and disk
                self.memory.store(output.clone());
                if let Some(disk) = &self.disk {
                    let _ = disk.store(&output);
                }
                return Ok(Some(output));
            }
        }
        Ok(None)
    }

    /// G1.13: Fetch from remote cache using a version-aware fingerprint key.
    ///
    /// Uses `fingerprint(task_id, version)` as the remote cache key. This
    /// ensures stale entries from previous toolchain versions are not fetched.
    pub fn get_remote_versioned(
        &self,
        id: &TaskId,
        version: &str,
    ) -> Result<Option<StoredOutput>, anyhow::Error> {
        if let Some(remote) = &self.remote {
            let fingerprint = fingerprint_task_id(id, version);
            let key = fingerprint.to_hex();
            if let Some(entry) = remote.get(&key)? {
                let output: StoredOutput = serde_json::from_str(&entry.code)?;

                // G12.37: Integrity verification
                let computed_hash: [u8; 16] = blake3::hash(&output.data).as_bytes()[..16]
                    .try_into()
                    .unwrap();
                if computed_hash != output.output_hash {
                    tracing::warn!(
                        "Remote cache integrity check failed for task {}: hash mismatch, discarding",
                        id,
                    );
                    return Ok(None);
                }

                // Promote to memory and disk
                self.memory.store(output.clone());
                if let Some(disk) = &self.disk {
                    let _ = disk.store(&output);
                }
                return Ok(Some(output));
            }
        }
        Ok(None)
    }

    /// Check if a task is in memory cache.
    pub fn is_in_memory(&self, id: &TaskId) -> bool {
        self.memory.contains(id)
    }

    /// Get the output hash for content-change detection.
    pub fn output_hash(&self, id: &TaskId) -> Option<[u8; 16]> {
        self.memory.output_hash(id)
    }

    /// Remove a task from all tiers.
    pub fn remove(&self, id: &TaskId) {
        self.memory.remove(id);
        if let Some(disk) = &self.disk {
            let _ = disk.remove(id);
        }
    }

    /// Clear all caches.
    pub fn clear(&self) {
        self.memory.clear();
        if let Some(disk) = &self.disk {
            let _ = disk.clear();
        }
    }

    /// Number of outputs in memory cache.
    pub fn memory_len(&self) -> usize {
        self.memory.len()
    }

    /// Get all task IDs in memory.
    pub fn memory_ids(&self) -> Vec<TaskId> {
        self.memory.ids()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_store_and_get() {
        let backend = MemoryBackend::new();
        let id = TaskId::compute("test", b"input");
        let output = StoredOutput::new(id, &"hello world".to_string(), vec![]).unwrap();

        backend.store(output);
        let retrieved = backend.get(&id).unwrap();
        let value: String = retrieved.deserialize().unwrap();
        assert_eq!(value, "hello world");
    }

    #[test]
    fn memory_backend_miss_returns_none() {
        let backend = MemoryBackend::new();
        let id = TaskId::compute("test", b"input");
        assert!(backend.get(&id).is_none());
    }

    #[test]
    fn memory_backend_remove() {
        let backend = MemoryBackend::new();
        let id = TaskId::compute("test", b"input");
        let output = StoredOutput::new(id, &42u32, vec![]).unwrap();
        backend.store(output);
        assert!(backend.contains(&id));
        backend.remove(&id);
        assert!(!backend.contains(&id));
    }

    #[test]
    fn disk_backend_store_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = DiskBackend::new(tmp.path().to_path_buf()).unwrap();
        let id = TaskId::compute("test", b"input");
        let output = StoredOutput::new(id, &"disk test".to_string(), vec![]).unwrap();

        backend.store(&output).unwrap();
        let retrieved = backend.get(&id).unwrap().unwrap();
        let value: String = retrieved.deserialize().unwrap();
        assert_eq!(value, "disk test");
    }

    #[test]
    fn disk_backend_miss_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = DiskBackend::new(tmp.path().to_path_buf()).unwrap();
        let id = TaskId::compute("test", b"input");
        assert!(backend.get(&id).unwrap().is_none());
    }

    #[test]
    fn three_tier_lookup_memory_first() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = DiskBackend::new(tmp.path().to_path_buf()).unwrap();
        let backend = TaskBackend::new(MemoryBackend::new()).with_disk(disk);

        let id = TaskId::compute("test", b"input");
        let output = StoredOutput::new(id, &"three tier".to_string(), vec![]).unwrap();
        backend.store(output);

        // Should find in memory
        let retrieved = backend.get(&id).unwrap();
        let value: String = retrieved.deserialize().unwrap();
        assert_eq!(value, "three tier");
    }

    #[test]
    fn three_tier_lookup_disk_on_memory_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = DiskBackend::new(tmp.path().to_path_buf()).unwrap();
        let memory = MemoryBackend::new();

        let id = TaskId::compute("test", b"input");
        let output = StoredOutput::new(id, &"disk fallback".to_string(), vec![]).unwrap();

        // Store to disk only (not memory)
        disk.store(&output).unwrap();

        let backend = TaskBackend::new(memory).with_disk(disk);

        // Should find in disk and promote to memory
        let retrieved = backend.get(&id).unwrap();
        let value: String = retrieved.deserialize().unwrap();
        assert_eq!(value, "disk fallback");

        // Should now be in memory
        assert!(backend.is_in_memory(&id));
    }

    #[test]
    fn stored_output_tracks_dependencies() {
        let dep1 = TaskId::compute("dep1", b"a");
        let dep2 = TaskId::compute("dep2", b"b");
        let id = TaskId::compute("parent", b"c");
        let output = StoredOutput::new(id, &"result".to_string(), vec![dep1, dep2]).unwrap();
        assert_eq!(output.dependencies, vec![dep1, dep2]);
    }

    #[test]
    fn disk_backend_integrity_check_passes_for_valid_data() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = DiskBackend::new(tmp.path().to_path_buf()).unwrap();
        let id = TaskId::compute("integrity_ok", b"input");
        let output = StoredOutput::new(id, &"legit data".to_string(), vec![]).unwrap();

        backend.store(&output).unwrap();

        // Valid data should load fine
        let retrieved = backend.get(&id).unwrap().unwrap();
        let value: String = retrieved.deserialize().unwrap();
        assert_eq!(value, "legit data");
    }

    #[test]
    fn disk_backend_integrity_check_rejects_tampered_data() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = DiskBackend::new(tmp.path().to_path_buf()).unwrap();
        let id = TaskId::compute("integrity_tampered", b"input");

        // Create a valid output, then tamper with the data on disk
        let mut output = StoredOutput::new(id, &"original".to_string(), vec![]).unwrap();
        backend.store(&output).unwrap();

        // Tamper: change the data but keep the old hash
        output.data = serde_json::to_vec(&"tampered".to_string()).unwrap();
        let path = backend.path_for(&id);
        let tampered_json = serde_json::to_vec_pretty(&output).unwrap();
        std::fs::write(&path, &tampered_json).unwrap();

        // Should return None (integrity check failed) and remove the file
        let result = backend.get(&id).unwrap();
        assert!(result.is_none(), "tampered entry should be rejected");

        // The corrupted file should have been removed
        assert!(!path.exists(), "corrupted file should be deleted");
    }

    #[test]
    fn fingerprint_task_id_is_version_aware() {
        let id = TaskId::compute("test_task", b"input");
        let fp1 = fingerprint_task_id(&id, "v1.0");
        let fp2 = fingerprint_task_id(&id, "v2.0");
        let fp1_again = fingerprint_task_id(&id, "v1.0");

        // Same version → same fingerprint
        assert_eq!(fp1, fp1_again, "Same version should produce same fingerprint");
        // Different version → different fingerprint
        assert_ne!(fp1, fp2, "Different versions should produce different fingerprints");
        // Fingerprint should differ from raw task ID
        assert_ne!(fp1, id, "Fingerprint should differ from raw task ID");
    }
}

/// G1.13: Compute a version-aware fingerprint for a TaskId.
///
/// The fingerprint is `blake3(task_id_bytes ++ 0xFC ++ version_bytes)` truncated
/// to 16 bytes. This is used as the remote cache key so that when the toolchain
/// version changes, stale remote cache entries are not fetched.
pub fn fingerprint_task_id(id: &TaskId, version: &str) -> TaskId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(id.as_bytes());
    hasher.update(&[0xFC]);
    hasher.update(version.as_bytes());
    let hash = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    TaskId::from_bytes(bytes)
}

#[cfg(test)]
mod lru_tests {
    use super::*;

    #[test]
    fn lru_eviction_evicts_oldest_first() {
        let backend = MemoryBackend::new();
        let id_a = TaskId::compute("lru_a", b"");
        let id_b = TaskId::compute("lru_b", b"");
        let id_c = TaskId::compute("lru_c", b"");

        backend.store(StoredOutput::new(id_a, &1u32, vec![]).unwrap());
        backend.store(StoredOutput::new(id_b, &2u32, vec![]).unwrap());
        backend.store(StoredOutput::new(id_c, &3u32, vec![]).unwrap());

        // Access A and B to make C the LRU
        let _ = backend.get(&id_a);
        let _ = backend.get(&id_b);

        // Evict 1 — should evict C (oldest access)
        let evicted = backend.evict_to_max(2);
        assert_eq!(evicted, 1, "Should evict 1 entry");
        assert!(!backend.contains(&id_c), "C should be evicted (LRU)");
        assert!(backend.contains(&id_a), "A should still be cached");
        assert!(backend.contains(&id_b), "B should still be cached");
    }

    #[test]
    fn lru_eviction_to_max_zero_evicts_all() {
        let backend = MemoryBackend::new();
        let id_a = TaskId::compute("evict_all_a", b"");
        let id_b = TaskId::compute("evict_all_b", b"");

        backend.store(StoredOutput::new(id_a, &1u32, vec![]).unwrap());
        backend.store(StoredOutput::new(id_b, &2u32, vec![]).unwrap());

        let evicted = backend.evict_to_max(0);
        assert_eq!(evicted, 2, "Should evict all 2 entries");
        assert!(backend.is_empty(), "Cache should be empty");
    }

    #[test]
    fn lru_eviction_noop_when_under_limit() {
        let backend = MemoryBackend::new();
        backend.store(StoredOutput::new(TaskId::compute("noop", b""), &1u32, vec![]).unwrap());

        let evicted = backend.evict_to_max(10);
        assert_eq!(evicted, 0, "Should not evict when under limit");
    }
}
