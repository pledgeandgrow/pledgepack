// Remote cache backend for sharing transform cache across machines.
//
// Supports content-addressable storage via:
//   - HTTP REST API (generic, self-hosted)
//   - S3-compatible storage (AWS S3, MinIO, Cloudflare R2, etc.)
//   - GCS (Google Cloud Storage)
//
// The remote cache is used as a fallback when the local disk cache
// misses. This enables CI builds to share cache across runs and
// team members to share cache across machines.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Validate that a URL is safe to use (no shell metacharacters, must be http/https).
/// This guards against command injection when URLs are passed to subprocesses
/// (e.g. `aws s3 cp` / `gsutil cp`) and ensures only http(s) schemes reach the
/// HTTP client.
fn validate_url(url: &str) -> Result<()> {
    // Reject shell metacharacters that could enable injection when the URL is
    // forwarded to a CLI subprocess (S3/GCS backends).
    check_no_shell_metachars(url)?;
    // For the HTTP backend, require an explicit http(s) scheme.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("URL must use http or https scheme: {}", url);
    }
    Ok(())
}

/// Validate an object-store URL (s3:// or gs://) for use with CLI subprocesses.
fn validate_object_url(url: &str) -> Result<()> {
    check_no_shell_metachars(url)?;
    if !(url.starts_with("s3://") || url.starts_with("gs://")) {
        bail!("Object URL must use s3 or gs scheme: {}", url);
    }
    Ok(())
}

fn check_no_shell_metachars(url: &str) -> Result<()> {
    const SHELL_METACHARS: &[char] = &[';', '|', '&', '$', '`', '(', ')', '<', '>', '\n', '\r', '*',
        '?', '[', ']', '{', '}', '!', '#', '~', '"', '\'', '\\', ' '];
    if url.contains(SHELL_METACHARS) {
        bail!("URL contains forbidden shell metacharacters: {}", url);
    }
    Ok(())
}

/// Configuration for the remote cache
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteCacheConfig {
    /// Backend type: "http", "s3", "gcs"
    pub backend: String,
    /// URL/endpoint (e.g., "https://cache.example.com" or "https://s3.amazonaws.com")
    pub endpoint: String,
    /// Bucket name (for S3/GCS)
    pub bucket: Option<String>,
    /// Region (for S3)
    pub region: Option<String>,
    /// Access key (for S3/GCS)
    pub access_key: Option<String>,
    /// Secret key (for S3/GCS)
    pub secret_key: Option<String>,
    /// Namespace prefix for cache keys (e.g., "myproject/")
    pub namespace: Option<String>,
    /// Request timeout in seconds
    pub timeout_secs: u64,
    /// Whether remote cache is enabled
    pub enabled: bool,
}

impl Default for RemoteCacheConfig {
    fn default() -> Self {
        Self {
            backend: "http".to_string(),
            endpoint: String::new(),
            bucket: None,
            region: None,
            access_key: None,
            secret_key: None,
            namespace: None,
            timeout_secs: 30,
            enabled: false,
        }
    }
}

/// A cached entry stored remotely (same structure as local CacheEntry)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteCacheEntry {
    pub code: String,
    pub source_map: Option<String>,
    pub deps: Vec<String>,
    pub created_at: u64,
}

/// Remote cache client — abstracts over HTTP/S3/GCS backends
pub struct RemoteCache {
    config: RemoteCacheConfig,
    enabled: bool,
}

impl RemoteCache {
    pub fn new(config: RemoteCacheConfig) -> Self {
        let enabled = config.enabled && !config.endpoint.is_empty();
        Self { config, enabled }
    }

    /// Check if remote cache is active
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get a cache entry from the remote backend
    pub fn get(&self, key: &str) -> Result<Option<RemoteCacheEntry>> {
        if !self.enabled {
            return Ok(None);
        }

        match self.config.backend.as_str() {
            "http" => self.http_get(key),
            "s3" => self.s3_get(key),
            "gcs" => self.gcs_get(key),
            _ => bail!("Unknown remote cache backend: {}", self.config.backend),
        }
    }

    /// Store a cache entry in the remote backend
    pub fn set(&self, key: &str, entry: &RemoteCacheEntry) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        match self.config.backend.as_str() {
            "http" => self.http_set(key, entry),
            "s3" => self.s3_set(key, entry),
            "gcs" => self.gcs_set(key, entry),
            _ => bail!("Unknown remote cache backend: {}", self.config.backend),
        }
    }

    fn build_url(&self, key: &str) -> String {
        let endpoint = self.config.endpoint.trim_end_matches('/');
        let ns = self
            .config
            .namespace
            .as_deref()
            .map(|n| n.trim_start_matches('/'))
            .unwrap_or("");
        if ns.is_empty() {
            format!("{}/cache/{}", endpoint, key)
        } else {
            format!("{}/cache/{}/{}", endpoint, ns, key)
        }
    }

    fn http_get(&self, key: &str) -> Result<Option<RemoteCacheEntry>> {
        let url = self.build_url(key);
        validate_url(&url)?;
        debug!("Remote cache GET: {}", url);

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(self.config.timeout_secs))
            .build()?;

        let resp = client.get(&url).send();
        match resp {
            Ok(response) if response.status().is_success() => {
                let body = response.bytes()?;
                if body.is_empty() {
                    debug!("Remote cache miss (empty body): {}", key);
                    return Ok(None);
                }
                match bincode::serde::decode_from_slice::<RemoteCacheEntry, _>(
                    &body,
                    bincode::config::standard(),
                ) {
                    Ok((entry, _)) => {
                        info!("Remote cache hit: {}", key);
                        Ok(Some(entry))
                    }
                    Err(e) => {
                        warn!("Remote cache deserialization failed: {}", e);
                        Ok(None)
                    }
                }
            }
            Ok(response) => {
                debug!(
                    "Remote cache miss (status {}): {}",
                    response.status(),
                    key
                );
                Ok(None)
            }
            Err(e) => {
                debug!("Remote cache GET error: {}: {}", key, e);
                Ok(None)
            }
        }
    }

    fn http_set(&self, key: &str, entry: &RemoteCacheEntry) -> Result<()> {
        let url = self.build_url(key);
        validate_url(&url)?;
        let data = bincode::serde::encode_to_vec(entry, bincode::config::standard())?;

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(self.config.timeout_secs))
            .build()?;

        let resp = client
            .put(&url)
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send();

        match resp {
            Ok(response) if response.status().is_success() => {
                debug!("Remote cache stored: {}", key);
                Ok(())
            }
            Ok(response) => Err(anyhow::anyhow!(
                "Remote cache store failed (status {}): {}",
                response.status(),
                key
            )),
            Err(e) => Err(anyhow::anyhow!("Remote cache store failed: {}", e)),
        }
    }

    fn s3_get(&self, key: &str) -> Result<Option<RemoteCacheEntry>> {
        let bucket = self.config.bucket.as_deref().unwrap_or("pledgepack-cache");
        let region = self.config.region.as_deref().unwrap_or("us-east-1");
        let ns = self.config.namespace.as_deref().unwrap_or("");
        let object_key = if ns.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", ns, key)
        };
        let s3_url = format!("s3://{}/{}", bucket, object_key);
        validate_object_url(&s3_url)?;

        let output = std::process::Command::new("aws")
            .args([
                "s3",
                "cp",
                &s3_url,
                "-",
                "--region",
                region,
            ])
            .output();

        match output {
            Ok(result) if result.status.success() && !result.stdout.is_empty() => {
                match bincode::serde::decode_from_slice::<RemoteCacheEntry, _>(
                    &result.stdout,
                    bincode::config::standard(),
                ) {
                    Ok((entry, _)) => {
                        info!("S3 cache hit: {}/{}", bucket, object_key);
                        Ok(Some(entry))
                    }
                    Err(e) => {
                        warn!("S3 cache deserialization failed: {}", e);
                        Ok(None)
                    }
                }
            }
            _ => {
                debug!("S3 cache miss: {}/{}", bucket, object_key);
                Ok(None)
            }
        }
    }

    fn s3_set(&self, key: &str, entry: &RemoteCacheEntry) -> Result<()> {
        let bucket = self.config.bucket.as_deref().unwrap_or("pledgepack-cache");
        let region = self.config.region.as_deref().unwrap_or("us-east-1");
        let ns = self.config.namespace.as_deref().unwrap_or("");
        let object_key = if ns.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", ns, key)
        };
        let s3_url = format!("s3://{}/{}", bucket, object_key);
        validate_object_url(&s3_url)?;

        let data = bincode::serde::encode_to_vec(entry, bincode::config::standard())?;
        let temp_file =
            std::env::temp_dir().join(format!("pledgepack_s3_{}", blake3::hash(&data).to_hex()));
        std::fs::write(&temp_file, &data)?;

        let output = std::process::Command::new("aws")
            .args([
                "s3",
                "cp",
                &temp_file.to_string_lossy(),
                &s3_url,
                "--region",
                region,
            ])
            .output();

        let _ = std::fs::remove_file(&temp_file);

        match output {
            Ok(result) if result.status.success() => {
                debug!("S3 cache stored: {}/{}", bucket, object_key);
                Ok(())
            }
            Ok(result) => Err(anyhow::anyhow!(
                "S3 cache store failed: {}",
                String::from_utf8_lossy(&result.stderr)
            )),
            Err(e) => Err(anyhow::anyhow!("S3 cache store failed: {}", e)),
        }
    }

    fn gcs_get(&self, key: &str) -> Result<Option<RemoteCacheEntry>> {
        let bucket = self.config.bucket.as_deref().unwrap_or("pledgepack-cache");
        let ns = self.config.namespace.as_deref().unwrap_or("");
        let object_key = if ns.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", ns, key)
        };
        let gs_url = format!("gs://{}/{}", bucket, object_key);
        validate_object_url(&gs_url)?;

        let output = std::process::Command::new("gsutil")
            .args(["cp", &gs_url, "-"])
            .output();

        match output {
            Ok(result) if result.status.success() && !result.stdout.is_empty() => {
                match bincode::serde::decode_from_slice::<RemoteCacheEntry, _>(
                    &result.stdout,
                    bincode::config::standard(),
                ) {
                    Ok((entry, _)) => {
                        info!("GCS cache hit: {}/{}", bucket, object_key);
                        Ok(Some(entry))
                    }
                    Err(e) => {
                        warn!("GCS cache deserialization failed: {}", e);
                        Ok(None)
                    }
                }
            }
            _ => {
                debug!("GCS cache miss: {}/{}", bucket, object_key);
                Ok(None)
            }
        }
    }

    fn gcs_set(&self, key: &str, entry: &RemoteCacheEntry) -> Result<()> {
        let bucket = self.config.bucket.as_deref().unwrap_or("pledgepack-cache");
        let ns = self.config.namespace.as_deref().unwrap_or("");
        let object_key = if ns.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", ns, key)
        };
        let gs_url = format!("gs://{}/{}", bucket, object_key);
        validate_object_url(&gs_url)?;

        let data = bincode::serde::encode_to_vec(entry, bincode::config::standard())?;
        let temp_file =
            std::env::temp_dir().join(format!("pledgepack_gcs_{}", blake3::hash(&data).to_hex()));
        std::fs::write(&temp_file, &data)?;

        let output = std::process::Command::new("gsutil")
            .args([
                "cp",
                &temp_file.to_string_lossy(),
                &gs_url,
            ])
            .output();

        let _ = std::fs::remove_file(&temp_file);

        match output {
            Ok(result) if result.status.success() => {
                debug!("GCS cache stored: {}/{}", bucket, object_key);
                Ok(())
            }
            Ok(result) => Err(anyhow::anyhow!(
                "GCS cache store failed: {}",
                String::from_utf8_lossy(&result.stderr)
            )),
            Err(e) => Err(anyhow::anyhow!("GCS cache store failed: {}", e)),
        }
    }
}

/// Build a remote cache key from a content hash and function ID
pub fn remote_cache_key(content_hash: u64, function_id: &str, path: &str) -> String {
    let combined = format!("{}:{}:{}", content_hash, function_id, path);
    let hash = blake3::hash(combined.as_bytes());
    hash.to_hex().as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remote_cache_key_deterministic() {
        let key1 = remote_cache_key(123, "transform", "/src/a.ts");
        let key2 = remote_cache_key(123, "transform", "/src/a.ts");
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_remote_cache_key_different_inputs() {
        let key1 = remote_cache_key(123, "transform", "/src/a.ts");
        let key2 = remote_cache_key(456, "transform", "/src/a.ts");
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_disabled_remote_cache() {
        let config = RemoteCacheConfig::default();
        let cache = RemoteCache::new(config);
        assert!(!cache.is_enabled());
    }
}
