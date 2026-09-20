//! Function-level incremental cache
//!
//! This is the "Turbo engine" equivalent — caches the result of
//! every function in the build pipeline. When a file changes,
//! only the affected functions are re-run.
//!
//! Two storage tiers:
//!   1. In-memory (dashmap) — fast, per-session
//!   2. Filesystem (bincode) — persistent across restarts

use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub mod advanced;
pub mod git_cache;
pub mod remote;
use tracing::debug;

/// Unique key for a cached function result
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    /// Content hash of the input file
    pub content_hash: u64,
    /// Function identifier (e.g., "swc_transform", "resolve_imports")
    pub function_id: String,
    /// Additional parameters that affect the output
    pub params_hash: u64,
}

/// Cached result of a function call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// The transformed/emitted code produced by the function.
    pub code: String,
    /// Optional source map accompanying `code`.
    pub source_map: Option<String>,
    /// Dependencies recorded while producing this entry; used for
    /// invalidation when any dependency changes.
    pub deps: Vec<String>,
    /// Unix timestamp (seconds) when the entry was created.
    pub created_at: u64,
    /// Schema version for forward compatibility. Entries with a mismatched
    /// version are treated as cache misses. Defaults to 0 for entries written
    /// by older versions (which lacked this field), so they are ignored.
    #[serde(default)]
    pub version: u32,
}

/// Current cache format version. Increment when the on-disk schema changes.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// The function-level cache
pub struct FunctionCache {
    /// In-memory cache (concurrent)
    memory: Arc<DashMap<CacheKey, CacheEntry>>,
    /// Cache directory on filesystem
    cache_dir: PathBuf,
    /// Whether filesystem persistence is enabled
    persist: bool,
}

impl FunctionCache {
    /// Create a new cache. When `persist` is true, entries are also written
    /// to `cache_dir` (created if missing) so they survive restarts;
    /// otherwise only the in-memory tier is used.
    pub fn new(cache_dir: PathBuf, persist: bool) -> Self {
        if persist && let Err(e) = std::fs::create_dir_all(&cache_dir) {
            tracing::warn!("Failed to create cache directory {:?}: {}", cache_dir, e);
        }

        Self {
            memory: Arc::new(DashMap::new()),
            cache_dir,
            persist,
        }
    }

    /// Get a cached entry
    pub fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        // Check memory first
        if let Some(entry) = self.memory.get(key) {
            debug!("Cache hit (memory): {}", key.function_id);
            return Some(entry.clone());
        }

        // Check filesystem
        if self.persist
            && let Ok(entry) = self.read_from_disk(key)
        {
            // Validate schema version — treat mismatches as cache misses so
            // stale entries from older formats are ignored rather than used.
            if entry.version != CACHE_FORMAT_VERSION {
                debug!(
                    "Cache entry version mismatch ({} != {}), ignoring: {}",
                    entry.version, CACHE_FORMAT_VERSION, key.function_id
                );
                return None;
            }
            debug!("Cache hit (disk): {}", key.function_id);
            // Populate memory cache
            self.memory.insert(key.clone(), entry.clone());
            return Some(entry);
        }

        None
    }

    /// Insert a cached entry
    pub fn set(&self, key: CacheKey, entry: CacheEntry) {
        self.memory.insert(key.clone(), entry.clone());

        if self.persist
            && let Err(e) = self.write_to_disk(&key, &entry)
        {
            tracing::warn!("Failed to persist cache entry: {}", e);
        }
    }

    /// Invalidate entries for a given content hash
    pub fn invalidate_by_content(&self, content_hash: u64) {
        let keys_to_remove: Vec<CacheKey> = self
            .memory
            .iter()
            .filter(|entry| entry.key().content_hash == content_hash)
            .map(|entry| entry.key().clone())
            .collect();

        for key in keys_to_remove {
            self.memory.remove(&key);
            if self.persist {
                let path = self.cache_path(&key);
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::debug!("Failed to remove cache file {:?}: {}", path, e);
                }
            }
        }
    }

    /// Clear the entire cache
    pub fn clear(&self) {
        self.memory.clear();
        if self.persist {
            if let Err(e) = std::fs::remove_dir_all(&self.cache_dir) {
                tracing::debug!("Failed to remove cache dir {:?}: {}", self.cache_dir, e);
            }
            if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
                tracing::warn!(
                    "Failed to recreate cache directory {:?}: {}",
                    self.cache_dir,
                    e
                );
            }
        }
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.memory.len() as u64,
        }
    }

    fn cache_path(&self, key: &CacheKey) -> PathBuf {
        let params_bytes = bincode::serde::encode_to_vec(key, bincode::config::standard())
            .unwrap_or_else(|e| {
                tracing::warn!("Failed to serialize cache key: {}", e);
                // Use debug representation as fallback to avoid hash collision
                format!("{:?}", key).into_bytes()
            });
        let hash = blake3::hash(&params_bytes);
        self.cache_dir.join(hash.to_hex().as_str())
    }

    fn read_from_disk(&self, key: &CacheKey) -> Result<CacheEntry> {
        let path = self.cache_path(key);
        let file = std::fs::File::open(&path)?;
        let metadata = file.metadata()?;

        // Use memmap2 for zero-copy reads of large cache entries
        let data = if metadata.len() > 4096 {
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            mmap.as_ref().to_vec()
        } else {
            // For small entries, direct read is faster than mmap setup
            std::fs::read(&path)?
        };

        let (entry, _) =
            bincode::serde::decode_from_slice::<CacheEntry, _>(&data, bincode::config::standard())?;
        Ok(entry)
    }

    fn write_to_disk(&self, key: &CacheKey, entry: &CacheEntry) -> Result<()> {
        let path = self.cache_path(key);
        let data = bincode::serde::encode_to_vec(entry, bincode::config::standard())?;
        // Atomic write: uniquely named temp file, then rename to final path
        atomic_write(&path, &data)?;
        Ok(())
    }
}

/// A collision-free sibling temp path for `target`: unique per process,
/// per call and per moment, so concurrent writers never share a temp file.
pub fn unique_temp_path(target: &std::path::Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{}.{}.{:x}.tmp", std::process::id(), n, nanos));
    target.with_file_name(name)
}

/// Atomically write `data` to `path`: write a uniquely named temp file
/// (created with `create_new`, so an existing file or symlink at that name is
/// never followed), then rename over `path`. The temp file is removed on error.
pub fn atomic_write(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = unique_temp_path(path);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        drop(f);
        // Windows can transiently refuse a replace-rename while another
        // writer/reader has the destination open; retry briefly.
        let mut attempt = 0;
        loop {
            match std::fs::rename(&tmp, path) {
                Ok(()) => break Ok(()),
                Err(e) if attempt < 50 && e.kind() == std::io::ErrorKind::PermissionDenied => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(e) => break Err(e),
            }
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Aggregate statistics about the cache, returned by
/// [`FunctionCache::stats`].
#[derive(Debug)]
pub struct CacheStats {
    /// Number of entries currently held in the in-memory tier.
    pub entries: u64,
}

/// Helper to compute a cache key
pub fn make_key(
    content_hash: u64,
    function_id: &str,
    params: &(impl serde::Serialize + std::fmt::Debug),
) -> CacheKey {
    let params_bytes = bincode::serde::encode_to_vec(params, bincode::config::standard())
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to serialize cache params: {}", e);
            // Use debug representation as fallback to avoid hash collision
            format!("{:?}", params).into_bytes()
        });
    let params_hash = u64::from_be_bytes(
        blake3::hash(&params_bytes).as_bytes()[0..8]
            .try_into()
            .unwrap(),
    );

    CacheKey {
        content_hash,
        function_id: function_id.to_string(),
        params_hash,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_set_get() {
        let dir = std::env::temp_dir().join("pledgepack_cache_test");
        let cache = FunctionCache::new(dir.clone(), false);

        let key = make_key(123, "test_fn", &"params");
        let entry = CacheEntry {
            code: "console.log('hello')".to_string(),
            source_map: None,
            deps: vec!["./foo".to_string()],
            created_at: 0,
            version: CACHE_FORMAT_VERSION,
        };

        cache.set(key.clone(), entry.clone());
        let result = cache.get(&key).unwrap();
        assert_eq!(result.code, entry.code);
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;

    #[test]
    fn unique_temp_paths_never_collide() {
        let p = std::path::Path::new("/tmp/x/entry.bin");
        let a = unique_temp_path(p);
        let b = unique_temp_path(p);
        assert_ne!(a, b);
        assert_eq!(a.parent(), p.parent());
    }

    #[test]
    fn concurrent_atomic_writes_to_one_path_all_succeed() {
        let dir = tempfile_dir("atomic_concurrent");
        let target = dir.join("entry.bin");
        let handles: Vec<_> = (0..16u8)
            .map(|i| {
                let target = target.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        atomic_write(&target, &[i; 4096]).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let data = std::fs::read(&target).unwrap();
        assert_eq!(data.len(), 4096);
        assert!(data.iter().all(|b| *b == data[0]), "torn write");
        // no stray temp files
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pledgepack_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
