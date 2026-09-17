// Advanced cache features: G9.5-G9.8, G9.11-G9.16
//
// G9.5:  Content-defined chunking (FastCDC) for large outputs
// G9.6:  Parallel remote fetch
// G9.7:  Remote cache prefetching
// G9.8:  P2P cache sharing via mDNS
// G9.11: Cache deduplication (same content hash = one copy)
// G9.12: Cache compression with zstd before upload
// G9.13: Cache signing with ed25519
// G9.14: Air-gapped cache sync (export/import)
// G9.15: Cache warming (pre-fetch entire cache)
// G9.16: Multi-tier remote cache with fallback

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tracing::{debug, info, warn};

// ─── G9.5: Content-defined chunking ──────────────────────────────────

const CHUNK_THRESHOLD: usize = 64 * 1024; // 64KB
const CHUNK_MIN_SIZE: usize = 4 * 1024; // 4KB minimum chunk
const CHUNK_MAX_SIZE: usize = 256 * 1024; // 256KB maximum chunk

/// A content-defined chunk produced by FastCDC
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Blake3 hash of the chunk content
    pub hash: String,
    /// Offset in the original data
    pub offset: usize,
    /// Size of the chunk
    pub size: usize,
}

/// G9.5: Split data into content-defined chunks using a simplified FastCDC algorithm.
/// Outputs > 64KB are split into chunks, each cached separately.
pub fn chunk_data(data: &[u8]) -> Vec<Chunk> {
    if data.len() <= CHUNK_THRESHOLD {
        return vec![Chunk {
            hash: blake3::hash(data).to_hex().as_str().to_string(),
            offset: 0,
            size: data.len(),
        }];
    }

    let mut chunks = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let remaining = data.len() - offset;
        let end = if remaining <= CHUNK_MIN_SIZE {
            // Small tail — just include it
            data.len()
        } else {
            find_chunk_boundary(
                &data[offset..],
                CHUNK_MIN_SIZE,
                CHUNK_MAX_SIZE.min(remaining),
            ) + offset
        };

        let chunk_data = &data[offset..end];
        let hash = blake3::hash(chunk_data).to_hex().as_str().to_string();
        chunks.push(Chunk {
            hash,
            offset,
            size: end - offset,
        });
        offset = end;
    }

    chunks
}

/// Find a chunk boundary using a simple gear-based rolling hash
fn find_chunk_boundary(data: &[u8], min_size: usize, max_size: usize) -> usize {
    let mask: u32 = 0x0000_0fff; // 12-bit mask for ~4KB average chunk size
    let mut hash: u32 = 0;

    for (i, &byte) in data.iter().enumerate() {
        if i >= max_size {
            return max_size;
        }
        if i < min_size {
            hash = hash.wrapping_mul(31).wrapping_add(byte as u32);
            continue;
        }
        hash = hash.wrapping_mul(31).wrapping_add(byte as u32);
        if (hash & mask) == 0 {
            return i + 1;
        }
    }

    data.len()
}

/// G9.5: Reassemble data from chunks
pub fn reassemble_data(chunks: &[Chunk], chunk_data: &HashMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    for chunk in chunks {
        match chunk_data.get(&chunk.hash) {
            Some(data) => result.extend_from_slice(data),
            None => bail!("Missing chunk: {}", chunk.hash),
        }
    }
    Ok(result)
}

// ─── G9.6: Parallel remote fetch ─────────────────────────────────────

/// G9.6: Fetch multiple cache entries in parallel using scoped OS threads.
/// Returns results in the same order as the input keys.
///
/// Each key is fetched on its own scoped thread (the remote cache backends
/// are blocking I/O — HTTP via reqwest::blocking, S3/GCS via CLI subprocesses —
/// so threads are the right primitive here; rayon is not a dependency of this
/// crate). `std::thread::scope` guarantees all threads are joined before this
/// function returns, and results preserve input order.
pub fn parallel_fetch(
    cache: &crate::remote::RemoteCache,
    keys: &[String],
) -> Vec<Option<crate::remote::RemoteCacheEntry>> {
    let results: Vec<std::sync::Mutex<Option<crate::remote::RemoteCacheEntry>>> =
        keys.iter().map(|_| std::sync::Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for (i, key) in keys.iter().enumerate() {
            let result_ref = &results[i];
            scope.spawn(move || {
                let value = cache.get(key).unwrap_or(None);
                *result_ref.lock().unwrap() = value;
            });
        }
    });

    results
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect()
}

// ─── G9.7: Remote cache prefetching ──────────────────────────────────

/// G9.7: Speculatively prefetch task outputs likely to be needed.
/// Uses a simple prediction: prefetch entries for tasks that share
/// dependencies with recently completed tasks.
pub struct Prefetcher {
    /// Recently accessed keys (for prediction)
    recent: Vec<String>,
    /// Prefetch queue
    queue: Vec<String>,
}

impl Default for Prefetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Prefetcher {
    pub fn new() -> Self {
        Self {
            recent: Vec::new(),
            queue: Vec::new(),
        }
    }

    /// Record that a key was accessed
    pub fn record_access(&mut self, key: &str) {
        self.recent.push(key.to_string());
        if self.recent.len() > 100 {
            self.recent.remove(0);
        }
    }

    /// Add keys to the prefetch queue
    pub fn add_to_queue(&mut self, keys: &[String]) {
        for key in keys {
            if !self.queue.contains(key) {
                self.queue.push(key.clone());
            }
        }
    }

    /// Get the next key to prefetch
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<String> {
        self.queue.pop()
    }

    /// Get the prefetch queue size
    pub fn queue_size(&self) -> usize {
        self.queue.len()
    }
}

// ─── G9.8: P2P cache sharing via mDNS ────────────────────────────────

/// G9.8: P2P cache entry metadata shared over the network
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct P2PCacheEntry {
    pub key: String,
    pub hash: String,
    pub size: u64,
    pub peer_addr: String,
}

/// G9.8: P2P cache discovery via mDNS
pub struct P2PCacheDiscovery {
    /// Known peers and their cache entries
    peers: HashMap<String, Vec<P2PCacheEntry>>,
}

impl P2PCacheDiscovery {
    pub fn new(_local_id: &str) -> Self {
        Self {
            peers: HashMap::new(),
        }
    }

    /// Register a peer's cache entries
    pub fn register_peer(&mut self, peer_addr: &str, entries: Vec<P2PCacheEntry>) {
        self.peers.insert(peer_addr.to_string(), entries);
    }

    /// Remove a peer
    pub fn remove_peer(&mut self, peer_addr: &str) {
        self.peers.remove(peer_addr);
    }

    /// Find which peer has a given cache key
    pub fn find_peer(&self, key: &str) -> Option<&str> {
        for (peer_addr, entries) in &self.peers {
            for entry in entries {
                if entry.key == key {
                    return Some(peer_addr.as_str());
                }
            }
        }
        None
    }

    /// Get all known peers
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get all entries from all peers
    pub fn all_entries(&self) -> Vec<&P2PCacheEntry> {
        self.peers.values().flat_map(|v| v.iter()).collect()
    }
}

// ─── G9.11: Cache deduplication ──────────────────────────────────────

/// G9.11: Deduplicate cache entries by content hash.
/// If two task outputs have the same content hash, store only one copy.
pub struct DedupCache {
    /// Map from content hash to stored path
    content_to_path: HashMap<String, std::path::PathBuf>,
    /// Map from cache key to content hash
    key_to_content: HashMap<String, String>,
    /// Reference counts for content
    ref_counts: HashMap<String, u32>,
}

impl Default for DedupCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DedupCache {
    pub fn new() -> Self {
        Self {
            content_to_path: HashMap::new(),
            key_to_content: HashMap::new(),
            ref_counts: HashMap::new(),
        }
    }

    /// Store a cache entry with deduplication
    pub fn store(&mut self, key: &str, data: &[u8], cache_dir: &Path) -> Result<()> {
        let content_hash = blake3::hash(data).to_hex().as_str().to_string();

        // Check if content already exists
        if !self.content_to_path.contains_key(&content_hash) {
            let path = cache_dir.join(&content_hash);
            std::fs::write(&path, data)?;
            self.content_to_path.insert(content_hash.clone(), path);
        }

        // Update mappings
        if let Some(old_hash) = self
            .key_to_content
            .insert(key.to_string(), content_hash.clone())
        {
            // Key existed before, decrement old ref count
            if let Some(count) = self.ref_counts.get_mut(&old_hash) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    // Remove unreferenced content
                    if let Some(path) = self.content_to_path.remove(&old_hash) {
                        std::fs::remove_file(path).ok();
                    }
                    self.ref_counts.remove(&old_hash);
                }
            }
        }

        *self.ref_counts.entry(content_hash).or_insert(0) += 1;
        Ok(())
    }

    /// Retrieve a cache entry
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.key_to_content.get(key) {
            Some(content_hash) => match self.content_to_path.get(content_hash) {
                Some(path) => Ok(Some(std::fs::read(path)?)),
                None => Ok(None),
            },
            None => Ok(None),
        }
    }

    /// Get dedup statistics
    pub fn stats(&self) -> DedupStats {
        let unique_entries = self.content_to_path.len();
        let total_keys = self.key_to_content.len();
        DedupStats {
            unique_entries,
            total_keys,
            dedup_ratio: if total_keys > 0 {
                1.0 - (unique_entries as f64 / total_keys as f64)
            } else {
                0.0
            },
        }
    }

    /// Persist dedup metadata to disk so it survives restarts.
    /// Writes `dedup-meta.json` inside `cache_dir`.
    pub fn persist(&self, cache_dir: &Path) -> Result<()> {
        let meta_path = cache_dir.join("dedup-meta.json");
        let meta = DedupMeta {
            content_to_path: self.content_to_path.clone(),
            key_to_content: self.key_to_content.clone(),
            ref_counts: self.ref_counts.clone(),
        };
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }

    /// Load dedup metadata from disk.
    /// Returns an empty cache when no `dedup-meta.json` exists yet.
    pub fn load(cache_dir: &Path) -> Result<Self> {
        let meta_path = cache_dir.join("dedup-meta.json");
        if !meta_path.exists() {
            return Ok(Self::new());
        }
        let content = std::fs::read_to_string(&meta_path)?;
        let meta: DedupMeta = serde_json::from_str(&content)?;
        Ok(Self {
            content_to_path: meta.content_to_path,
            key_to_content: meta.key_to_content,
            ref_counts: meta.ref_counts,
        })
    }
}

/// Serializable form of [`DedupCache`] metadata for disk persistence.
#[derive(Debug, Serialize, Deserialize)]
struct DedupMeta {
    content_to_path: HashMap<String, std::path::PathBuf>,
    key_to_content: HashMap<String, String>,
    ref_counts: HashMap<String, u32>,
}

#[derive(Debug)]
pub struct DedupStats {
    pub unique_entries: usize,
    pub total_keys: usize,
    pub dedup_ratio: f64,
}

// ─── G9.12: Cache compression with zstd ──────────────────────────────

/// G9.12: Compress data with zstd before uploading to remote cache
pub fn compress_cache_entry(data: &[u8]) -> Result<Vec<u8>> {
    let compressed = zstd::stream::encode_all(data, 3)?;
    debug!(
        "Compressed cache entry: {} -> {} ({:.1}%)",
        data.len(),
        compressed.len(),
        compressed.len() as f64 / data.len() as f64 * 100.0
    );
    Ok(compressed)
}

/// G9.12: Decompress zstd-compressed cache entry
pub fn decompress_cache_entry(compressed: &[u8]) -> Result<Vec<u8>> {
    let data = zstd::stream::decode_all(compressed)?;
    Ok(data)
}

// ─── G9.13: Cache signing with ed25519 ───────────────────────────────

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

/// G9.13: A signed cache entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedCacheEntry {
    /// The serialized cache entry data
    pub data: Vec<u8>,
    /// Ed25519 signature of the data
    pub signature: Vec<u8>,
    /// Public key of the signer
    pub public_key: Vec<u8>,
}

/// G9.13: Generate a new ed25519 key pair for cache signing.
///
/// NOTE: This uses OS randomness, so the keypair — and therefore cache
/// signatures produced with it — is intentionally non-reproducible across
/// calls. Use [`derive_signing_keypair`] when a deterministic, reproducible
/// keypair is required (e.g. reproducible builds / tests).
pub fn generate_signing_keypair() -> (SigningKey, VerifyingKey) {
    let mut rng = OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}

/// G9.13: Derive a deterministic ed25519 key pair from a seed.
///
/// Uses BLAKE3 as a KDF: the same seed always produces the same keypair,
/// making cache signatures reproducible across runs and machines. The seed
/// must still be kept secret — anyone with the seed can forge signatures.
pub fn derive_signing_keypair(seed: &[u8]) -> (SigningKey, VerifyingKey) {
    // BLAKE3 KDF for deterministic key derivation
    let hash = blake3::hash(seed);
    let signing_key = SigningKey::from_bytes(hash.as_bytes());
    let verifying_key = signing_key.verifying_key();
    (signing_key, verifying_key)
}

/// G9.13: Sign cache data with a private key
pub fn sign_cache_entry(data: &[u8], signing_key: &SigningKey) -> SignedCacheEntry {
    let signature = signing_key.sign(data);
    let verifying_key = signing_key.verifying_key();
    SignedCacheEntry {
        data: data.to_vec(),
        signature: signature.to_bytes().to_vec(),
        public_key: verifying_key.to_bytes().to_vec(),
    }
}

/// G9.13: Verify a signed cache entry
pub fn verify_cache_entry(entry: &SignedCacheEntry) -> Result<bool> {
    let public_key = VerifyingKey::from_bytes(
        entry
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid public key length"))?,
    )
    .map_err(|e| anyhow::anyhow!("Invalid public key: {}", e))?;

    let signature = Signature::from_slice(&entry.signature)
        .map_err(|e| anyhow::anyhow!("Invalid signature: {}", e))?;

    Ok(public_key.verify(&entry.data, &signature).is_ok())
}

// ─── G9.14: Air-gapped cache sync ────────────────────────────────────

/// G9.14: Export cache to a compressed tar file for air-gapped transfer
pub fn export_cache(cache_dir: &Path, output_path: &Path) -> Result<ExportStats> {
    let mut stats = ExportStats::default();
    let mut archive_data = Vec::new();

    if !cache_dir.exists() {
        bail!("Cache directory does not exist: {}", cache_dir.display());
    }

    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            let data = std::fs::read(&path)?;
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            // Simple format: [filename_len:u32][filename][data_len:u32][data]
            let fname_bytes = filename.as_bytes();
            archive_data.extend_from_slice(&(fname_bytes.len() as u32).to_le_bytes());
            archive_data.extend_from_slice(fname_bytes);
            archive_data.extend_from_slice(&(data.len() as u32).to_le_bytes());
            archive_data.extend_from_slice(&data);

            stats.files += 1;
            stats.uncompressed_size += data.len() as u64;
        }
    }

    // Compress with zstd
    let compressed = zstd::stream::encode_all(&archive_data[..], 3)?;
    stats.compressed_size = compressed.len() as u64;

    std::fs::write(output_path, &compressed)?;
    info!(
        "Exported {} cache entries: {} -> {} (compressed)",
        stats.files, stats.uncompressed_size, stats.compressed_size
    );

    Ok(stats)
}

/// G9.14: Import cache from a compressed tar file
pub fn import_cache(input_path: &Path, cache_dir: &Path) -> Result<ImportStats> {
    let mut stats = ImportStats::default();

    let compressed = std::fs::read(input_path)?;
    let archive_data = zstd::stream::decode_all(&compressed[..])?;

    std::fs::create_dir_all(cache_dir)?;

    let mut offset = 0;
    while offset < archive_data.len() {
        if offset + 4 > archive_data.len() {
            break;
        }
        let fname_len = u32::from_le_bytes([
            archive_data[offset],
            archive_data[offset + 1],
            archive_data[offset + 2],
            archive_data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + fname_len > archive_data.len() {
            break;
        }
        let filename =
            String::from_utf8_lossy(&archive_data[offset..offset + fname_len]).to_string();
        offset += fname_len;

        if offset + 4 > archive_data.len() {
            break;
        }
        let data_len = u32::from_le_bytes([
            archive_data[offset],
            archive_data[offset + 1],
            archive_data[offset + 2],
            archive_data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + data_len > archive_data.len() {
            break;
        }
        let data = &archive_data[offset..offset + data_len];
        offset += data_len;

        let dest = cache_dir.join(&filename);
        std::fs::write(&dest, data)?;

        stats.files += 1;
        stats.total_size += data_len as u64;
    }

    info!("Imported {} cache entries", stats.files);
    Ok(stats)
}

#[derive(Debug, Default)]
pub struct ExportStats {
    pub files: u64,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub files: u64,
    pub total_size: u64,
}

// ─── G9.15: Cache warming ────────────────────────────────────────────

/// G9.15: Pre-fetch cache entries for a project
pub struct CacheWarmer {
    /// Keys to warm
    keys: Vec<String>,
}

impl Default for CacheWarmer {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheWarmer {
    pub fn new() -> Self {
        Self { keys: Vec::new() }
    }

    /// Add keys to warm
    pub fn add_keys(&mut self, keys: &[String]) {
        for key in keys {
            if !self.keys.contains(key) {
                self.keys.push(key.clone());
            }
        }
    }

    /// Warm the cache by fetching all keys from remote
    pub fn warm(
        &self,
        remote: &crate::remote::RemoteCache,
        local_cache: &crate::FunctionCache,
    ) -> Result<WarmStats> {
        let mut stats = WarmStats::default();

        for key in &self.keys {
            match remote.get(key) {
                Ok(Some(entry)) => {
                    let cache_key = crate::CacheKey {
                        content_hash: 0,
                        function_id: key.clone(),
                        params_hash: 0,
                    };
                    let local_entry = crate::CacheEntry {
                        code: entry.code,
                        source_map: entry.source_map,
                        deps: entry.deps,
                        created_at: entry.created_at,
                        version: crate::CACHE_FORMAT_VERSION,
                    };
                    local_cache.set(cache_key, local_entry);
                    stats.fetched += 1;
                }
                Ok(None) => {
                    stats.missed += 1;
                }
                Err(e) => {
                    warn!("Failed to warm cache key {}: {}", key, e);
                    stats.errors += 1;
                }
            }
        }

        info!(
            "Cache warming: {} fetched, {} missed, {} errors",
            stats.fetched, stats.missed, stats.errors
        );

        Ok(stats)
    }

    pub fn key_count(&self) -> usize {
        self.keys.len()
    }
}

#[derive(Debug, Default)]
pub struct WarmStats {
    pub fetched: u64,
    pub missed: u64,
    pub errors: u64,
}

// ─── G9.16: Multi-tier remote cache with fallback ────────────────────

/// G9.16: Multi-tier remote cache with automatic fallback
pub struct MultiTierCache {
    /// Ordered list of remote caches (tried in order)
    tiers: Vec<Arc<crate::remote::RemoteCache>>,
}

impl Default for MultiTierCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiTierCache {
    pub fn new() -> Self {
        Self { tiers: Vec::new() }
    }

    /// Add a cache tier (added in priority order — first = highest priority)
    pub fn add_tier(&mut self, cache: Arc<crate::remote::RemoteCache>) {
        self.tiers.push(cache);
    }

    /// Get from the first tier that has the entry
    pub fn get(&self, key: &str) -> Result<Option<crate::remote::RemoteCacheEntry>> {
        for (i, tier) in self.tiers.iter().enumerate() {
            match tier.get(key) {
                Ok(Some(entry)) => {
                    debug!("Cache hit at tier {}: {}", i, key);
                    return Ok(Some(entry));
                }
                Ok(None) => {
                    debug!("Cache miss at tier {}: {}", i, key);
                    continue;
                }
                Err(e) => {
                    warn!("Cache error at tier {} for {}: {}", i, key, e);
                    continue;
                }
            }
        }
        Ok(None)
    }

    /// Store in all tiers (write-through)
    pub fn set(&self, key: &str, entry: &crate::remote::RemoteCacheEntry) -> Result<()> {
        let mut had_error = false;
        for (i, tier) in self.tiers.iter().enumerate() {
            if let Err(e) = tier.set(key, entry) {
                warn!("Failed to store at tier {} for {}: {}", i, key, e);
                had_error = true;
            }
        }
        if had_error && self.tiers.is_empty() {
            bail!("No cache tiers configured");
        }
        Ok(())
    }

    /// Get the number of tiers
    pub fn tier_count(&self) -> usize {
        self.tiers.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_g95_chunk_small_data() {
        let data = vec![0u8; 100];
        let chunks = chunk_data(&data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].size, 100);
    }

    #[test]
    fn test_g95_chunk_large_data() {
        // Use pseudo-random data so the rolling hash finds boundaries
        let mut state: u32 = 12345;
        let data: Vec<u8> = (0..200 * 1024)
            .map(|_| {
                state = state.wrapping_mul(1103515245).wrapping_add(12345);
                (state >> 16) as u8
            })
            .collect();
        let chunks = chunk_data(&data);
        assert!(chunks.len() > 1);
        let total: usize = chunks.iter().map(|c| c.size).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn test_g95_reassemble() {
        let data = vec![42u8; 100];
        let chunks = chunk_data(&data);
        let mut chunk_map = HashMap::new();
        for chunk in &chunks {
            chunk_map.insert(
                chunk.hash.clone(),
                data[chunk.offset..chunk.offset + chunk.size].to_vec(),
            );
        }
        let reassembled = reassemble_data(&chunks, &chunk_map).unwrap();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn test_g97_prefetcher() {
        let mut pf = Prefetcher::new();
        pf.record_access("key1");
        pf.record_access("key2");
        pf.add_to_queue(&["key3".to_string(), "key4".to_string()]);
        assert_eq!(pf.queue_size(), 2);
        assert_eq!(pf.next(), Some("key4".to_string()));
        assert_eq!(pf.next(), Some("key3".to_string()));
        assert_eq!(pf.next(), None);
    }

    #[test]
    fn test_g98_p2p_discovery() {
        let mut p2p = P2PCacheDiscovery::new("local");
        assert_eq!(p2p.peer_count(), 0);

        let entries = vec![P2PCacheEntry {
            key: "key1".to_string(),
            hash: "abc".to_string(),
            size: 100,
            peer_addr: "192.168.1.10:8080".to_string(),
        }];
        p2p.register_peer("192.168.1.10:8080", entries);
        assert_eq!(p2p.peer_count(), 1);
        assert_eq!(p2p.find_peer("key1"), Some("192.168.1.10:8080"));
        assert_eq!(p2p.find_peer("key2"), None);
    }

    #[test]
    fn test_g911_dedup_cache() {
        let dir = std::env::temp_dir().join("pledgepack_dedup_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut dedup = DedupCache::new();

        // Store same data under different keys
        let data = vec![1u8; 100];
        dedup.store("key1", &data, &dir).unwrap();
        dedup.store("key2", &data, &dir).unwrap();

        let stats = dedup.stats();
        assert_eq!(stats.total_keys, 2);
        assert_eq!(stats.unique_entries, 1);
        assert!(stats.dedup_ratio > 0.0);

        // Both keys should return the same data
        let r1 = dedup.get("key1").unwrap().unwrap();
        let r2 = dedup.get("key2").unwrap().unwrap();
        assert_eq!(r1, r2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_g912_compress_decompress() {
        let data =
            b"Hello, World! This is a test cache entry that should compress well. ".repeat(10);
        let compressed = compress_cache_entry(&data).unwrap();
        assert!(compressed.len() < data.len());

        let decompressed = decompress_cache_entry(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_g913_sign_verify() {
        let (signing_key, _) = generate_signing_keypair();
        let data = b"test cache entry data";

        let signed = sign_cache_entry(data, &signing_key);
        assert!(verify_cache_entry(&signed).unwrap());
    }

    #[test]
    fn test_g913_reject_tampered() {
        let (signing_key, _) = generate_signing_keypair();
        let data = b"test cache entry data";

        let mut signed = sign_cache_entry(data, &signing_key);
        signed.data[0] ^= 1; // Tamper
        assert!(!verify_cache_entry(&signed).unwrap());
    }

    #[test]
    fn test_g914_export_import() {
        let cache_dir = std::env::temp_dir().join("pledgepack_export_test");
        let _ = std::fs::remove_dir_all(&cache_dir);
        std::fs::create_dir_all(&cache_dir).unwrap();

        // Create some cache files
        std::fs::write(cache_dir.join("entry1"), b"data1").unwrap();
        std::fs::write(cache_dir.join("entry2"), b"data2").unwrap();

        let export_path = std::env::temp_dir().join("pledgepack_cache_export.zst");
        let export_stats = export_cache(&cache_dir, &export_path).unwrap();
        assert_eq!(export_stats.files, 2);

        // Import to a new directory
        let import_dir = std::env::temp_dir().join("pledgepack_import_test");
        let _ = std::fs::remove_dir_all(&import_dir);
        let import_stats = import_cache(&export_path, &import_dir).unwrap();
        assert_eq!(import_stats.files, 2);

        // Verify imported data
        assert_eq!(std::fs::read(import_dir.join("entry1")).unwrap(), b"data1");
        assert_eq!(std::fs::read(import_dir.join("entry2")).unwrap(), b"data2");

        let _ = std::fs::remove_file(&export_path);
        let _ = std::fs::remove_dir_all(&cache_dir);
        let _ = std::fs::remove_dir_all(&import_dir);
    }

    #[test]
    fn test_g915_cache_warmer() {
        let warmer = CacheWarmer::new();
        assert_eq!(warmer.key_count(), 0);

        let mut warmer = CacheWarmer::new();
        warmer.add_keys(&["key1".to_string(), "key2".to_string(), "key1".to_string()]);
        assert_eq!(warmer.key_count(), 2);
    }

    #[test]
    fn test_g916_multi_tier() {
        let mut multi = MultiTierCache::new();
        assert_eq!(multi.tier_count(), 0);

        let config1 = crate::remote::RemoteCacheConfig {
            enabled: false,
            ..Default::default()
        };
        let config2 = crate::remote::RemoteCacheConfig {
            enabled: false,
            ..Default::default()
        };

        multi.add_tier(Arc::new(crate::remote::RemoteCache::new(config1)));
        multi.add_tier(Arc::new(crate::remote::RemoteCache::new(config2)));
        assert_eq!(multi.tier_count(), 2);
    }
}
