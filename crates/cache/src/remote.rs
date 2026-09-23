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
    const SHELL_METACHARS: &[char] = &[
        ';', '|', '&', '$', '`', '(', ')', '<', '>', '\n', '\r', '*', '?', '[', ']', '{', '}', '!',
        '#', '~', '"', '\'', '\\', ' ',
    ];
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
#[derive(Clone)]
pub struct RemoteCache {
    config: RemoteCacheConfig,
    enabled: bool,
    /// Shared HTTP client, built once. `reqwest::blocking::Client` spins up
    /// its own internal Tokio runtime (plus OS proxy autodiscovery) on every
    /// `build()`; a build is a real build-time cost, not the request's, was
    /// paying that cost on every single `get`/`set` call — up to two per
    /// module per build. On loopback that is invisible in isolation but
    /// compounds badly under CPU contention (hundreds of runtime spin-ups
    /// competing with the rest of the build for threads). One client is
    /// reused for the cache's whole lifetime instead; `reqwest::blocking::
    /// Client` is `Clone` (cheap, `Arc`-backed) so this struct stays `Clone`.
    /// `None` when the client failed to build (e.g. TLS backend init
    /// failure) — every call then behaves as a miss/no-op, same as disabled.
    client: Option<reqwest::blocking::Client>,
}

impl RemoteCache {
    pub fn new(config: RemoteCacheConfig) -> Self {
        let enabled = config.enabled && !config.endpoint.is_empty();
        let client = if enabled {
            match reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(config.timeout_secs))
                .connect_timeout(std::time::Duration::from_secs(config.timeout_secs.min(10)))
                // A build cache endpoint is a project-configured, trusted
                // destination, not general web traffic — it should never be
                // routed through a corporate/system HTTP proxy. This also
                // sidesteps a well-known reqwest-on-Windows stall: without
                // `no_proxy()`, the client's first request (in some
                // environments, every request) can block for many seconds
                // to tens of seconds on a WinHTTP/WPAD proxy-autoconfig
                // lookup before it ever reaches the network — reproduced
                // against the real dev-server-adjacent build path even
                // though an isolated loopback test of this same code
                // completed in well under a second (see the tests below).
                .no_proxy()
                .build()
            {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!(
                        "Remote cache: failed to build HTTP client, disabling: {}",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
        let enabled = enabled && client.is_some();
        Self {
            config,
            enabled,
            client,
        }
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

        let Some(client) = self.client.as_ref() else {
            return Ok(None);
        };

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
                debug!("Remote cache miss (status {}): {}", response.status(), key);
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

        let Some(client) = self.client.as_ref() else {
            bail!("remote cache client unavailable");
        };

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
            .args(["s3", "cp", &s3_url, "-", "--region", region])
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
        // Upload from stdin (`aws s3 cp - <url>`): no temp file at all, so
        // there is no predictable path for another local user to pre-create
        // or symlink-swap.
        let output = run_with_stdin(
            "aws",
            &["s3", "cp", "-", &s3_url, "--region", region],
            &data,
        );

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
        // Upload from stdin (`gsutil cp - <url>`); see s3_set.
        let output = run_with_stdin("gsutil", &["cp", "-", &gs_url], &data);

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
/// Run `program args...` feeding `data` on stdin, returning its output.
/// The data is written from a separate thread so a child that produces
/// output before draining stdin cannot deadlock us.
fn run_with_stdin(
    program: &str,
    args: &[&str],
    data: &[u8],
) -> std::io::Result<std::process::Output> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let payload = data.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
        // dropping `stdin` closes the pipe -> EOF for the child
    });
    let output = child.wait_with_output();
    let _ = writer.join();
    output
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

#[cfg(test)]
mod hang_repro {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A minimal HTTP/1.1 server (no framework): stores PUT bodies keyed by
    /// path, serves them back on GET, 404 otherwise. Runs until the test
    /// process exits.
    fn spawn_mock_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let store: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>> =
                std::sync::Mutex::new(std::collections::HashMap::new());
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 8192];
                let mut header_end = None;
                let mut data = Vec::new();
                while header_end.is_none() {
                    let Ok(n) = stream.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                    header_end = data.windows(4).position(|w| w == b"\r\n\r\n");
                }
                let Some(pos) = header_end else { continue };
                let head = String::from_utf8_lossy(&data[..pos]).to_string();
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let content_len: usize = lines
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().to_string())
                    })
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = data[pos + 4..].to_vec();
                while body.len() < content_len {
                    let Ok(n) = stream.read(&mut buf) else { break };
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&buf[..n]);
                }
                // Every response carries `Connection: close`: this server
                // handles exactly one request per socket and then drops it.
                // Without the header, a keep-alive client (reqwest) pools the
                // connection and may dispatch the NEXT request onto the dead
                // socket before noticing the FIN — a race that lost the GET
                // on ARM64 CI (`GET returned no entry` right after a
                // successful PUT).
                let mut s = store.lock().unwrap();
                if method == "PUT" {
                    s.insert(path, body);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                } else if method == "GET" {
                    if let Some(b) = s.get(&path) {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            b.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(b);
                    } else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                } else {
                    let _ = stream.write_all(
                        b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        });
        port
    }

    #[test]
    fn http_round_trip_completes_within_five_seconds() {
        let port = spawn_mock_server();
        let config = RemoteCacheConfig {
            backend: "http".to_string(),
            endpoint: format!("http://127.0.0.1:{port}"),
            enabled: true,
            timeout_secs: 5,
            ..Default::default()
        };
        let cache = RemoteCache::new(config);
        let entry = RemoteCacheEntry {
            code: "console.log(1)".to_string(),
            source_map: None,
            deps: vec![],
            created_at: 0,
        };

        let cache2 = cache.clone();
        let entry2 = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let set_ok = cache2.set("k1", &entry2).is_ok();
            let got = cache2.get("k1").ok().flatten();
            let _ = tx.send((set_ok, got));
        });
        let (set_ok, got) = rx.recv_timeout(std::time::Duration::from_secs(5)).expect(
            "remote cache set/get did not complete within 5s on a local loopback \
                 server — a per-request reqwest::blocking::Client with default OS proxy \
                 detection is the known cause on Windows (WinHTTP proxy autodiscovery)",
        );
        assert!(set_ok, "PUT to mock remote cache failed");
        assert_eq!(got.expect("GET returned no entry").code, entry.code);
    }

    #[test]
    fn many_sequential_requests_stay_fast() {
        let port = spawn_mock_server();
        let config = RemoteCacheConfig {
            backend: "http".to_string(),
            endpoint: format!("http://127.0.0.1:{port}"),
            enabled: true,
            timeout_secs: 5,
            ..Default::default()
        };
        let cache = RemoteCache::new(config);
        let entry = RemoteCacheEntry {
            code: "x".to_string(),
            source_map: None,
            deps: vec![],
            created_at: 0,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            for i in 0..50 {
                let key = format!("k{i}");
                let _ = cache.set(&key, &entry);
                let _ = cache.get(&key);
            }
            let _ = tx.send(start.elapsed());
        });
        let elapsed = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("50 sequential remote cache round trips did not finish within 20s");
        assert!(
            elapsed.as_millis() < 5000,
            "50 round trips to a local server took {elapsed:?} — expected well under 5s; \
             a fresh reqwest client per call is too slow for a per-module cache path"
        );
    }
}
