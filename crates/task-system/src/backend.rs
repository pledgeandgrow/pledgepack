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
use serde::{Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
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
    pub fn new<T: Serialize>(
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
    /// Create an empty in-memory backend.
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
        self.lru
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
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
        let ts = self
            .lru_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.lru
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, ts);
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
    /// Create a disk backend rooted at `cache_dir`, creating the `tasks`
    /// subdirectory if it doesn't already exist.
    pub fn new(cache_dir: PathBuf) -> std::io::Result<Self> {
        let tasks_dir = cache_dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir)?;
        Ok(DiskBackend {
            cache_dir: cache_dir.join("tasks"),
        })
    }

    fn path_for(&self, id: &TaskId) -> PathBuf {
        self.cache_dir.join(format!("{}.json", id.to_hex()))
    }

    /// Store a task output to disk (atomic write via temp-file-then-rename).
    pub fn store(&self, output: &StoredOutput) -> std::io::Result<()> {
        let path = self.path_for(&output.task_id);
        let data = serde_json::to_vec_pretty(output).map_err(std::io::Error::other)?;
        // Uniquely named temp file + rename: concurrent stores never share one.
        pledgepack_cache::atomic_write(&path, &data)?;
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

    /// Check whether an output exists on disk without reading or
    /// deserializing it. Used for cache-hit metrics where `get()`'s full
    /// read + integrity re-hash would be wasted work.
    pub fn contains(&self, id: &TaskId) -> bool {
        self.path_for(id).exists()
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
            if let Some(hex) = name.strip_suffix(".json")
                && let Some(id) = TaskId::from_hex(hex)
            {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

/// Content-addressed disk backend — the default disk tier.
///
/// Layout under `<cache_dir>/cas/`:
///
/// ```text
/// objects/<xx>/<rest-of-hex>.zst   zstd-compressed bincode StoredOutput,
///                                  named by blake3 of the compressed blob
/// index.log                        append-only "<task_hex> <blob_hex>\n"
///                                  (tombstone: "<task_hex> -")
/// ```
///
/// The `TaskId` is the lookup key; the blob hash is the storage key — two
/// different tasks with identical serialized outputs share one object
/// (dedup for free), and a read is integrity-verified by construction: a
/// corrupted or tampered blob can't match its own address. Blobs are read
/// through `memmap2` so large outputs page in lazily.
///
/// The index log is append-only: stores append a line, removes append a
/// tombstone, and `compact()` rewrites the log with live entries only.
/// `gc()` deletes objects not referenced by the index.
pub struct CasBackend {
    /// `<cache_dir>/cas`
    dir: PathBuf,
    /// `dir/objects`
    objects: PathBuf,
    /// task id → blob hash, replayed from index.log at open
    index: RwLock<HashMap<TaskId, [u8; 32]>>,
    /// Handle to `dir/index.lock`. The mutex serialises threads of this
    /// process; the OS advisory lock on the file (shared for appends,
    /// exclusive for compaction) serialises processes. Lock order is always
    /// `index_lock` -> `index`, never the reverse.
    index_lock: Mutex<std::fs::File>,
}

impl CasBackend {
    /// Create (or open) a CAS backend rooted at `cache_dir`.
    pub fn new(cache_dir: PathBuf) -> std::io::Result<Self> {
        let dir = cache_dir.join("cas");
        let objects = dir.join("objects");
        std::fs::create_dir_all(&objects)?;

        let index = Self::replay_index(&dir.join("index.log"));
        // Make sure index.log exists so appenders can rely on it.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.log"))?;
        let index_lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join("index.lock"))?;

        Ok(CasBackend {
            dir,
            objects,
            index: RwLock::new(index),
            index_lock: Mutex::new(index_lock),
        })
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("index.log")
    }

    /// Replay the append-only index log; last entry wins, `-` is a tombstone.
    fn replay_index(path: &PathBuf) -> HashMap<TaskId, [u8; 32]> {
        let mut index = HashMap::new();
        let Ok(data) = std::fs::read_to_string(path) else {
            return index;
        };
        for line in data.lines() {
            let mut parts = line.split_whitespace();
            let (Some(task_hex), Some(blob_hex)) = (parts.next(), parts.next()) else {
                continue; // skip malformed lines
            };
            let Some(task_id) = TaskId::from_hex(task_hex) else {
                continue;
            };
            if blob_hex == "-" {
                index.remove(&task_id);
                continue;
            }
            if let Some(hash) = parse_blob_hash(blob_hex) {
                index.insert(task_id, hash);
            }
        }
        index
    }

    /// Path for a blob: objects/<first-2-hex>/<remaining-62-hex>.zst
    fn object_path(&self, hash: &[u8; 32]) -> PathBuf {
        let hex = to_hex64(hash);
        self.objects
            .join(&hex[..2])
            .join(format!("{}.zst", &hex[2..]))
    }

    /// Append one line to index.log and apply `update` to the in-memory
    /// index, both while holding the index lock, so a concurrent
    /// `compact_index` can neither miss the line nor drop it.
    fn append_index(
        &self,
        line: &str,
        update: impl FnOnce(&mut HashMap<TaskId, [u8; 32]>),
    ) -> std::io::Result<()> {
        use std::io::Write;

        let guard = self.index_lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.lock_shared()?;
        let result = (|| {
            // Opened per append (not held open) so a log replaced by another
            // process's compaction is never appended to through a stale handle.
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.log_path())?;
            log.write_all(line.as_bytes())
        })();
        if result.is_ok() {
            update(&mut self.index.write().unwrap_or_else(|e| e.into_inner()));
        }
        let _ = guard.unlock();
        result
    }

    /// Store a task output. Content-identical outputs share one blob.
    pub fn store(&self, output: &StoredOutput) -> std::io::Result<()> {
        let raw = bincode::serde::encode_to_vec(output, bincode::config::standard())
            .map_err(std::io::Error::other)?;
        let compressed = zstd::stream::encode_all(&raw[..], 3)?;
        let hash: [u8; 32] = *blake3::hash(&compressed).as_bytes();

        let path = self.object_path(&hash);
        if !path.exists() {
            // Dedup: only write new content. Atomic via a *uniquely named*
            // temp file + rename, so concurrent stores never share or
            // clobber a temp file.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            pledgepack_cache::atomic_write(&path, &compressed)?;
        }

        let task_id = output.task_id;
        let line = format!("{} {}\n", task_id.to_hex(), to_hex64(&hash));
        self.append_index(&line, |index| {
            index.insert(task_id, hash);
        })?;
        debug!("Stored task output to CAS: {}", output.task_id);
        Ok(())
    }

    /// Load a task output. Verifies the blob's blake3 against its address
    /// before decompressing — a corrupted blob is a miss, not a crash.
    pub fn get(&self, id: &TaskId) -> std::io::Result<Option<StoredOutput>> {
        let Some(hash) = self
            .index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
        else {
            return Ok(None);
        };

        let path = self.object_path(&hash);
        if !path.exists() {
            // Index references a missing object — drop the stale entry.
            self.index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            return Ok(None);
        }

        // mmap the blob — lazily paged, zero-copy into the decompressor's
        // read path for large outputs.
        let file = std::fs::File::open(&path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let compressed: &[u8] = &mmap[..];

        // Integrity: the address IS the hash.
        if *blake3::hash(compressed).as_bytes() != hash {
            tracing::warn!(
                "CAS integrity check failed for task {} (blob {}): discarding",
                id,
                to_hex64(&hash),
            );
            drop(mmap);
            let _ = std::fs::remove_file(&path);
            self.index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
            return Ok(None);
        }

        let raw = zstd::stream::decode_all(compressed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        drop(mmap);
        let (output, _): (StoredOutput, usize) =
            bincode::serde::decode_from_slice(&raw, bincode::config::standard())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        trace!("Loaded task output from CAS: {}", id);
        Ok(Some(output))
    }

    /// Check whether an output exists without reading it.
    pub fn contains(&self, id: &TaskId) -> bool {
        self.index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }

    /// Tombstone a task output. The blob stays — it may be shared.
    pub fn remove(&self, id: &TaskId) -> std::io::Result<()> {
        if !self.contains(id) {
            return Ok(());
        }
        let task_id = *id;
        self.append_index(&format!("{} -\n", id.to_hex()), |index| {
            index.remove(&task_id);
        })
    }

    /// Rewrite index.log with only live entries (drops tombstones and
    /// superseded lines). Call after heavy invalidation churn.
    ///
    /// Safe against concurrent stores/removes, in this process and others:
    /// the whole rewrite runs under the exclusive index lock (appenders hold
    /// it shared), the live set is rebuilt from the on-disk log — so lines
    /// appended by other processes are kept — and the new log is swapped in
    /// with an atomic rename of a uniquely named temp file.
    pub fn compact_index(&self) -> std::io::Result<()> {
        use std::fmt::Write as _;

        let guard = self.index_lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.lock()?;
        let result = (|| {
            let live = Self::replay_index(&self.log_path());
            let mut body = String::with_capacity(live.len() * 100);
            for (id, hash) in live.iter() {
                let _ = writeln!(body, "{} {}", id.to_hex(), to_hex64(hash));
            }
            pledgepack_cache::atomic_write(&self.log_path(), body.as_bytes())?;
            *self.index.write().unwrap_or_else(|e| e.into_inner()) = live;
            Ok(())
        })();
        let _ = guard.unlock();
        result
    }

    /// Delete object blobs not referenced by the index.
    /// Returns (objects removed, bytes reclaimed).
    pub fn gc(&self) -> std::io::Result<(usize, u64)> {
        let live: std::collections::HashSet<String> = self
            .index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(to_hex64)
            .collect();

        let mut removed = 0usize;
        let mut bytes = 0u64;
        for sub in std::fs::read_dir(&self.objects)? {
            let sub = sub?;
            if !sub.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(sub.path())? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some(rest) = name.strip_suffix(".zst") else {
                    continue;
                };
                let full = format!("{}{}", sub.file_name().to_string_lossy(), rest);
                if !live.contains(&full) {
                    bytes += entry.metadata()?.len();
                    std::fs::remove_file(entry.path())?;
                    removed += 1;
                }
            }
        }
        Ok((removed, bytes))
    }

    /// Clear all entries and objects.
    pub fn clear(&self) -> std::io::Result<()> {
        let guard = self.index_lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.lock()?;
        let result = (|| {
            self.index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            if self.objects.exists() {
                std::fs::remove_dir_all(&self.objects)?;
                std::fs::create_dir_all(&self.objects)?;
            }
            std::fs::File::create(self.log_path())?;
            Ok(())
        })();
        let _ = guard.unlock();
        result
    }

    /// All cached task IDs.
    pub fn ids(&self) -> Vec<TaskId> {
        self.index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect()
    }
}

/// Lowercase hex of a 32-byte blob hash (64 chars).
fn to_hex64(hash: &[u8; 32]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse a 64-char hex blob hash.
fn parse_blob_hash(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

/// The disk tier — JSON-per-key (legacy) or content-addressed (default).
///
/// `CasBackend` is the default for new engines; `DiskBackend` stays
/// available for migration and comparison.
pub enum DiskTier {
    /// Legacy per-task JSON files (`tasks/<id>.json`).
    Json(DiskBackend),
    /// Content-addressed store (`cas/`).
    Cas(CasBackend),
}

impl DiskTier {
    /// Store a task output.
    pub fn store(&self, output: &StoredOutput) -> std::io::Result<()> {
        match self {
            DiskTier::Json(d) => d.store(output),
            DiskTier::Cas(d) => d.store(output),
        }
    }

    /// Load a task output.
    pub fn get(&self, id: &TaskId) -> std::io::Result<Option<StoredOutput>> {
        match self {
            DiskTier::Json(d) => d.get(id),
            DiskTier::Cas(d) => d.get(id),
        }
    }

    /// Existence check without a full read.
    pub fn contains(&self, id: &TaskId) -> bool {
        match self {
            DiskTier::Json(d) => d.contains(id),
            DiskTier::Cas(d) => d.contains(id),
        }
    }

    /// Remove an entry.
    pub fn remove(&self, id: &TaskId) -> std::io::Result<()> {
        match self {
            DiskTier::Json(d) => d.remove(id),
            DiskTier::Cas(d) => d.remove(id),
        }
    }

    /// Clear all entries.
    pub fn clear(&self) -> std::io::Result<()> {
        match self {
            DiskTier::Json(d) => d.clear(),
            DiskTier::Cas(d) => d.clear(),
        }
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
    /// The in-memory tier, always present.
    pub memory: MemoryBackend,
    disk: Option<DiskTier>,
    remote: Option<pledgepack_cache::remote::RemoteCache>,
}

impl TaskBackend {
    /// Wrap a memory backend with no disk or remote tier configured.
    pub fn new(memory: MemoryBackend) -> Self {
        TaskBackend {
            memory,
            disk: None,
            remote: None,
        }
    }

    /// Enable the legacy JSON disk tier.
    pub fn with_disk(mut self, disk: DiskBackend) -> Self {
        self.disk = Some(DiskTier::Json(disk));
        self
    }

    /// Enable the content-addressed disk tier (the default for new engines).
    pub fn with_cas(mut self, cas: CasBackend) -> Self {
        self.disk = Some(DiskTier::Cas(cas));
        self
    }

    /// Enable a pre-built disk tier.
    pub fn with_disk_tier(mut self, tier: DiskTier) -> Self {
        self.disk = Some(tier);
        self
    }

    /// Enable the remote tier.
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
        if let Some(disk) = &self.disk
            && let Ok(Some(output)) = disk.get(id)
        {
            // Promote to memory
            self.memory.store(output.clone());
            return Some(Arc::new(output));
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

    /// Get the disk tier (if configured).
    pub fn disk(&self) -> Option<&DiskTier> {
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
        if let Some(disk) = &self.disk
            && let Err(e) = disk.store(&output)
        {
            tracing::warn!("Failed to store task output to disk: {}", e);
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
                deps: output.dependencies.iter().map(|d| d.to_hex()).collect(),
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
                deps: output.dependencies.iter().map(|d| d.to_hex()).collect(),
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

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn cas_backend_store_and_get() {
        let tmp = tmpdir();
        let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        let id = TaskId::compute("test", b"cas_input");
        let output = StoredOutput::new(id, &"hello cas".to_string(), vec![]).unwrap();

        assert!(!cas.contains(&id));
        cas.store(&output).unwrap();
        assert!(cas.contains(&id));

        let got = cas.get(&id).unwrap().unwrap();
        assert_eq!(got.deserialize::<String>().unwrap(), "hello cas");
    }

    #[test]
    fn cas_backend_repeated_store_is_idempotent() {
        let tmp = tmpdir();
        let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        let a = TaskId::compute("test", b"task_a");
        let b = TaskId::compute("test", b"task_b");
        let out_a = StoredOutput::new(a, &42u32, vec![]).unwrap();
        let out_b = StoredOutput::new(b, &42u32, vec![]).unwrap();

        let count_objects = |dir: &std::path::Path| -> usize {
            std::fs::read_dir(dir.join("cas/objects"))
                .unwrap()
                .map(|sub| std::fs::read_dir(sub.unwrap().path()).unwrap().count())
                .sum()
        };

        // Storing the same content twice still yields one blob — the
        // content-address makes re-stores no-ops.
        cas.store(&out_a).unwrap();
        cas.store(&out_a).unwrap();
        assert_eq!(count_objects(tmp.path()), 1);

        // A different StoredOutput (task_id is embedded in the blob) is a
        // second object.
        cas.store(&out_b).unwrap();
        assert_eq!(count_objects(tmp.path()), 2);
        assert_eq!(cas.get(&a).unwrap().unwrap().task_id, a);
        assert_eq!(cas.get(&b).unwrap().unwrap().task_id, b);
    }

    #[test]
    fn cas_backend_persists_across_instances() {
        let tmp = tmpdir();
        let id = TaskId::compute("test", b"persistent");
        {
            let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
            cas.store(&StoredOutput::new(id, &"value".to_string(), vec![]).unwrap())
                .unwrap();
        }
        // Reopen — the index log replays.
        let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        assert!(cas.contains(&id));
        assert_eq!(
            cas.get(&id)
                .unwrap()
                .unwrap()
                .deserialize::<String>()
                .unwrap(),
            "value"
        );
    }

    #[test]
    fn cas_backend_tombstone_survives_reopen() {
        let tmp = tmpdir();
        let id = TaskId::compute("test", b"tombstone");
        {
            let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
            cas.store(&StoredOutput::new(id, &1u32, vec![]).unwrap())
                .unwrap();
            cas.remove(&id).unwrap();
        }
        let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        assert!(!cas.contains(&id));
        assert!(cas.get(&id).unwrap().is_none());
    }

    #[test]
    fn cas_backend_gc_removes_unreferenced_blobs() {
        let tmp = tmpdir();
        let keep = TaskId::compute("test", b"keep");
        let drop_id = TaskId::compute("test", b"drop");
        let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        cas.store(&StoredOutput::new(keep, &1u32, vec![]).unwrap())
            .unwrap();
        cas.store(&StoredOutput::new(drop_id, &2u32, vec![]).unwrap())
            .unwrap();
        cas.remove(&drop_id).unwrap();

        let (removed, _bytes) = cas.gc().unwrap();
        assert_eq!(removed, 1);
        assert!(cas.contains(&keep));
        assert!(cas.get(&keep).unwrap().is_some());
    }

    #[test]
    fn cas_backend_corrupted_blob_is_a_miss() {
        let tmp = tmpdir();
        let id = TaskId::compute("test", b"corrupt");
        let cas = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        cas.store(&StoredOutput::new(id, &"data".to_string(), vec![]).unwrap())
            .unwrap();

        // Corrupt every stored blob.
        for sub in std::fs::read_dir(tmp.path().join("cas/objects")).unwrap() {
            for entry in std::fs::read_dir(sub.unwrap().path()).unwrap() {
                let entry = entry.unwrap();
                std::fs::write(entry.path(), b"garbage").unwrap();
            }
        }

        assert!(cas.get(&id).unwrap().is_none());
        assert!(!cas.contains(&id));
    }

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

#[cfg(test)]
mod cas_concurrency_tests {
    use super::*;
    use std::sync::Arc;

    fn out(i: u32) -> StoredOutput {
        StoredOutput::new(TaskId::compute("t", &i.to_le_bytes()), &i, vec![]).unwrap()
    }

    /// `compact_index` used to snapshot the index and then swap the log under a
    /// different lock than appenders used, silently dropping lines appended in
    /// between. Hammer stores against repeated compactions and reopen from disk.
    #[test]
    fn compact_index_does_not_lose_concurrent_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(CasBackend::new(tmp.path().to_path_buf()).unwrap());
        let writers: Vec<_> = (0..4u32)
            .map(|w| {
                let cas = cas.clone();
                std::thread::spawn(move || {
                    for i in 0..60u32 {
                        cas.store(&out(w * 1000 + i)).unwrap();
                    }
                })
            })
            .collect();
        let compactor = {
            let cas = cas.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    cas.compact_index().unwrap();
                }
            })
        };
        for w in writers {
            w.join().unwrap();
        }
        compactor.join().unwrap();
        cas.compact_index().unwrap();
        assert_eq!(cas.ids().len(), 240);

        // A fresh open replays the on-disk log: nothing was lost there either.
        let reopened = CasBackend::new(tmp.path().to_path_buf()).unwrap();
        assert_eq!(reopened.ids().len(), 240);
        for w in 0..4u32 {
            for i in 0..60u32 {
                let id = TaskId::compute("t", &(w * 1000 + i).to_le_bytes());
                assert!(reopened.get(&id).unwrap().is_some());
            }
        }
    }

    #[test]
    fn concurrent_stores_of_same_content_share_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cas = Arc::new(CasBackend::new(tmp.path().to_path_buf()).unwrap());
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let cas = cas.clone();
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        cas.store(&out(7)).unwrap();
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(cas.get(&out(7).task_id).unwrap().is_some());
        fn stray_tmp(dir: &std::path::Path) -> usize {
            let mut n = 0;
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    n += stray_tmp(&p);
                } else if p.extension().is_some_and(|x| x == "tmp") {
                    n += 1;
                }
            }
            n
        }
        assert_eq!(stray_tmp(tmp.path()), 0);
    }
}
