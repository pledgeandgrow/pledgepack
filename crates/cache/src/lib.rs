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
/// v2: transform `deps` now includes CJS `require("x")` specifiers — v1
/// entries would silently drop those edges from the module graph.
pub const CACHE_FORMAT_VERSION: u32 = 2;

/// Where one persisted entry lives inside a pack file.
#[derive(Debug, Clone, Copy)]
struct Loc {
    pack: usize,
    off: u64,
    len: u32,
}

/// One row of a pack's index file.
#[derive(Serialize, Deserialize)]
struct IndexRow {
    key: [u8; 32],
    off: u64,
    len: u32,
}

/// A pack: many encoded entries in one `.dat` file plus a `.idx` listing them.
/// Writing one file per build instead of one file per entry is what keeps the
/// disk tier cheap on filesystems where file creation is slow (Windows).
struct Pack {
    dat: PathBuf,
    idx: PathBuf,
    mmap: std::sync::OnceLock<Option<memmap2::Mmap>>,
}

impl Pack {
    fn new(dat: PathBuf, idx: PathBuf) -> Self {
        Self {
            dat,
            idx,
            mmap: std::sync::OnceLock::new(),
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        self.mmap
            .get_or_init(|| {
                let f = std::fs::File::open(&self.dat).ok()?;
                // SAFETY: packs are immutable once committed (the .idx is written
                // last); a concurrent compaction only ever deletes whole files.
                unsafe { memmap2::Mmap::map(&f).ok() }
            })
            .as_deref()
    }
}

#[derive(Default)]
struct DiskIndex {
    map: std::collections::HashMap<[u8; 32], Loc>,
    packs: Vec<Pack>,
}

/// Pending disk writes are flushed once they reach this many bytes.
const FLUSH_BYTES: usize = 32 * 1024 * 1024;
/// When this many packs exist, they are merged into one.
const COMPACT_PACKS: usize = 16;

type PendingEntry = ([u8; 32], u64, Vec<u8>);

/// The function-level cache
pub struct FunctionCache {
    /// In-memory cache (concurrent)
    memory: Arc<DashMap<CacheKey, CacheEntry>>,
    /// Cache directory on filesystem
    cache_dir: PathBuf,
    /// Whether filesystem persistence is enabled
    persist: bool,
    /// Committed packs on disk.
    disk: std::sync::RwLock<DiskIndex>,
    /// Encoded entries not yet written (key hash, content hash, bytes) and
    /// their total size.
    pending: std::sync::Mutex<(Vec<PendingEntry>, usize)>,
}

impl FunctionCache {
    /// Create a new cache. When `persist` is true, entries are also written
    /// to `cache_dir` (created if missing) so they survive restarts;
    /// otherwise only the in-memory tier is used.
    ///
    /// Persisted entries are buffered and written as a single pack when the
    /// cache is [`flush`](Self::flush)ed or dropped.
    pub fn new(cache_dir: PathBuf, persist: bool) -> Self {
        let mut disk = DiskIndex::default();
        if persist {
            if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                tracing::warn!("Failed to create cache directory {:?}: {}", cache_dir, e);
            }
            load_packs(&cache_dir, &mut disk);
        }

        Self {
            memory: Arc::new(DashMap::new()),
            cache_dir,
            persist,
            disk: std::sync::RwLock::new(disk),
            pending: std::sync::Mutex::new((Vec::new(), 0)),
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
            && let Some(entry) = self.read_from_disk(key)
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
        if self.persist {
            match bincode::serde::encode_to_vec(&entry, bincode::config::standard()) {
                Ok(bytes) => {
                    let khash = key_hash(&key);
                    let flush_now = {
                        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                        p.1 += bytes.len();
                        p.0.push((khash, key.content_hash, bytes));
                        p.1 >= FLUSH_BYTES
                    };
                    if flush_now {
                        self.flush();
                    }
                }
                Err(e) => tracing::warn!("Failed to encode cache entry: {}", e),
            }
        }
        self.memory.insert(key, entry);
    }

    /// Write all buffered entries to disk as one pack. Called automatically
    /// on drop; call it explicitly to make entries visible to other
    /// processes earlier.
    pub fn flush(&self) {
        if !self.persist {
            return;
        }
        let batch = {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.1 = 0;
            std::mem::take(&mut p.0)
        };
        if batch.is_empty() {
            return;
        }
        if let Err(e) = self.write_pack(batch) {
            tracing::warn!("Failed to persist cache pack: {}", e);
        }
    }

    fn write_pack(&self, batch: Vec<PendingEntry>) -> Result<()> {
        let mut data = Vec::with_capacity(batch.iter().map(|b| b.2.len()).sum());
        let mut rows = Vec::with_capacity(batch.len());
        for (key, _, bytes) in &batch {
            rows.push(IndexRow {
                key: *key,
                off: data.len() as u64,
                len: bytes.len() as u32,
            });
            data.extend_from_slice(bytes);
        }
        std::fs::create_dir_all(&self.cache_dir)?;
        let pack = write_pack_files(&self.cache_dir, &data, &rows)?;

        let mut disk = self.disk.write().unwrap_or_else(|e| e.into_inner());
        let id = disk.packs.len();
        disk.packs.push(pack);
        for r in rows {
            disk.map.insert(
                r.key,
                Loc {
                    pack: id,
                    off: r.off,
                    len: r.len,
                },
            );
        }
        if disk.packs.len() >= COMPACT_PACKS {
            self.compact(&mut disk);
        }
        Ok(())
    }

    /// Merge every live entry into a single new pack and delete the old ones.
    fn compact(&self, disk: &mut DiskIndex) {
        let mut data = Vec::new();
        let mut rows = Vec::with_capacity(disk.map.len());
        for (key, loc) in disk.map.iter() {
            let Some(bytes) = disk.packs.get(loc.pack).and_then(|p| p.bytes()) else {
                continue;
            };
            let (start, end) = (loc.off as usize, loc.off as usize + loc.len as usize);
            let Some(slice) = bytes.get(start..end) else {
                continue;
            };
            rows.push(IndexRow {
                key: *key,
                off: data.len() as u64,
                len: loc.len,
            });
            data.extend_from_slice(slice);
        }
        let merged = match write_pack_files(&self.cache_dir, &data, &rows) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("Cache compaction skipped: {}", e);
                return;
            }
        };
        let old = std::mem::take(&mut disk.packs);
        disk.map.clear();
        disk.packs.push(merged);
        for r in rows {
            disk.map.insert(
                r.key,
                Loc {
                    pack: 0,
                    off: r.off,
                    len: r.len,
                },
            );
        }
        for p in old {
            let (idx, dat) = (p.idx.clone(), p.dat.clone());
            drop(p);
            // Index first, so an interrupted cleanup never leaves an index
            // pointing at a missing data file.
            let _ = std::fs::remove_file(&idx);
            let _ = std::fs::remove_file(&dat);
        }
    }

    /// Invalidate entries for a given content hash
    pub fn invalidate_by_content(&self, content_hash: u64) {
        // Persisted entries are content-addressed, so a stale one can never be
        // returned for changed content; only the session-local copies (and any
        // not-yet-written batch) need to go.
        self.memory.retain(|k, _| k.content_hash != content_hash);
        let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.0.retain(|(_, c, _)| *c != content_hash);
        p.1 = p.0.iter().map(|e| e.2.len()).sum();
    }

    /// Clear the entire cache
    pub fn clear(&self) {
        self.memory.clear();
        {
            let mut p = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            p.0.clear();
            p.1 = 0;
        }
        if self.persist {
            let mut disk = self.disk.write().unwrap_or_else(|e| e.into_inner());
            *disk = DiskIndex::default();
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

    fn read_from_disk(&self, key: &CacheKey) -> Option<CacheEntry> {
        let khash = key_hash(key);
        let disk = self.disk.read().unwrap_or_else(|e| e.into_inner());
        let loc = *disk.map.get(&khash)?;
        let bytes = disk.packs.get(loc.pack)?.bytes()?;
        let slice = bytes.get(loc.off as usize..loc.off as usize + loc.len as usize)?;
        bincode::serde::decode_from_slice::<CacheEntry, _>(slice, bincode::config::standard())
            .ok()
            .map(|(e, _)| e)
    }
}

impl Drop for FunctionCache {
    fn drop(&mut self) {
        self.flush();
    }
}

fn key_hash(key: &CacheKey) -> [u8; 32] {
    let params_bytes = bincode::serde::encode_to_vec(key, bincode::config::standard())
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to serialize cache key: {}", e);
            // Use debug representation as fallback to avoid hash collision
            format!("{:?}", key).into_bytes()
        });
    *blake3::hash(&params_bytes).as_bytes()
}

fn pack_stem() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("pack-{:032x}-{}", nanos, std::process::id())
}

/// Write `data` and its index `rows` as a new pack in `dir`. The `.dat` goes
/// first and the `.idx` last: a pack without an index is ignored.
fn write_pack_files(dir: &std::path::Path, data: &[u8], rows: &[IndexRow]) -> Result<Pack> {
    let stem = pack_stem();
    let dat = dir.join(format!("{stem}.dat"));
    let idx = dir.join(format!("{stem}.idx"));
    atomic_write(&dat, data)?;
    let idx_bytes = bincode::serde::encode_to_vec(rows, bincode::config::standard())?;
    if let Err(e) = atomic_write(&idx, &idx_bytes) {
        let _ = std::fs::remove_file(&dat);
        return Err(e.into());
    }
    Ok(Pack::new(dat, idx))
}

/// Load every committed pack (one that has both `.idx` and `.dat`) in the
/// order they were written, so newer entries win.
fn load_packs(dir: &std::path::Path, disk: &mut DiskIndex) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut idxs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "idx"))
        .collect();
    idxs.sort();
    for idx in idxs {
        let dat = idx.with_extension("dat");
        if !dat.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read(&idx) else {
            continue;
        };
        let Ok((rows, _)) = bincode::serde::decode_from_slice::<Vec<IndexRow>, _>(
            &raw,
            bincode::config::standard(),
        ) else {
            continue;
        };
        let id = disk.packs.len();
        disk.packs.push(Pack::new(dat, idx));
        for r in rows {
            disk.map.insert(
                r.key,
                Loc {
                    pack: id,
                    off: r.off,
                    len: r.len,
                },
            );
        }
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

#[cfg(test)]
mod pack_tests {
    use super::*;

    fn entry(code: &str) -> CacheEntry {
        CacheEntry {
            code: code.to_string(),
            source_map: Some("{}".to_string()),
            deps: vec!["./a".to_string()],
            created_at: 1,
            version: CACHE_FORMAT_VERSION,
        }
    }

    fn dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pledgepack_pack_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn idx_count(d: &std::path::Path) -> usize {
        std::fs::read_dir(d)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "idx")
            })
            .count()
    }

    #[test]
    fn entries_survive_reopen_and_use_few_files() {
        let d = dir("reopen");
        {
            let c = FunctionCache::new(d.clone(), true);
            for i in 0..500u64 {
                c.set(make_key(i, "t", &"p"), entry(&format!("code{i}")));
            }
        } // drop flushes
        let files = std::fs::read_dir(&d).unwrap().count();
        assert_eq!(files, 2, "500 entries must land in one .dat + one .idx");
        let c = FunctionCache::new(d.clone(), true);
        for i in 0..500u64 {
            assert_eq!(
                c.get(&make_key(i, "t", &"p")).unwrap().code,
                format!("code{i}")
            );
        }
        assert!(c.get(&make_key(9999, "t", &"p")).is_none());
        drop(c);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn many_builds_compact_into_few_packs_without_losing_entries() {
        let d = dir("compact");
        let rounds = COMPACT_PACKS as u64 * 2 + 3;
        for round in 0..rounds {
            let c = FunctionCache::new(d.clone(), true);
            c.set(make_key(round, "t", &"p"), entry(&format!("r{round}")));
        }
        assert!(idx_count(&d) < COMPACT_PACKS, "expected compaction");
        let c = FunctionCache::new(d.clone(), true);
        for round in 0..rounds {
            assert_eq!(
                c.get(&make_key(round, "t", &"p")).unwrap().code,
                format!("r{round}")
            );
        }
        drop(c);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pack_without_index_is_ignored() {
        let d = dir("torn");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("pack-1-1.dat"), b"garbage").unwrap();
        let c = FunctionCache::new(d.clone(), true);
        assert!(c.get(&make_key(1, "t", &"p")).is_none());
        drop(c);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn invalidate_drops_unflushed_entries_and_clear_empties_disk() {
        let d = dir("inval");
        let c = FunctionCache::new(d.clone(), true);
        c.set(make_key(1, "t", &"p"), entry("one"));
        c.set(make_key(2, "t", &"p"), entry("two"));
        c.invalidate_by_content(1);
        assert!(c.get(&make_key(1, "t", &"p")).is_none());
        c.flush();
        c.clear();
        assert!(c.get(&make_key(2, "t", &"p")).is_none());
        drop(c);
        let c = FunctionCache::new(d.clone(), true);
        assert!(c.get(&make_key(2, "t", &"p")).is_none());
        drop(c);
        let _ = std::fs::remove_dir_all(&d);
    }
}
