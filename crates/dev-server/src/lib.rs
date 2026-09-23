//! Dev server with HMR support
//!
//! Serves modules on-demand (lazy bundling like Turbopack):
//!   1. Browser requests / → serve index.html
//!   2. index.html loads /src/index.tsx → transform on-the-fly with Oxc
//!   3. Import specifiers rewritten to browser-compatible URLs
//!   4. File changes → notify watcher → WebSocket push → HMR update

use anyhow::Result;
use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use pledgepack_core::diagnostics as pledge_diagnostics;
use pledgepack_core::module::ModuleKind;
use pledgepack_core::transform as pledge_transform;
use pledgepack_core::{BuildEngine, PledgeConfig, normalize_path, normalize_path_str};
use pledgepack_js_plugin_host::JsPluginHost;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, mpsc};
use tracing::{info, warn};

pub use plugin_hooks::PluginHookService;

mod fs_guard;
mod hmr_diff;
mod lazy_pipeline;
mod middleware;
mod origin_guard;
mod plugin_hooks;
mod shell_generator;
mod watcher;

/// Maximum number of entries in the transformed-module cache before eviction.
const MAX_MODULE_CACHE_SIZE: usize = 1000;
/// Maximum number of entries in the HMR import graph before eviction.
const MAX_IMPORT_GRAPH_SIZE: usize = 1000;
/// Maximum response body size for files served by the dev server (100 MB).
const MAX_RESPONSE_SIZE: usize = 100 * 1024 * 1024;

/// Insert into a bounded `HashMap`, evicting an existing entry when at capacity.
///
/// This is an approximate (amortized) bound rather than a true LRU — `HashMap`
/// keeps no insertion order and the `lru` crate is not a dependency, so we
/// evict the first key yielded by the map's iterator. Existing keys are
/// refreshed in place without counting against the limit.
fn bounded_insert<V>(map: &mut HashMap<String, V>, key: String, value: V, max_entries: usize) {
    if !map.contains_key(&key) && map.len() >= max_entries {
        // Evict an entry to make room (approximate eviction — see doc comment)
        if let Some(evict_key) = map.keys().next().cloned() {
            map.remove(&evict_key);
        }
    }
    map.insert(key, value);
}

/// Canonicalize `path`, walking up to its nearest existing ancestor and
/// re-appending the (non-existent) trailing components if `path` itself
/// doesn't exist. Falls back to `path` unchanged if no ancestor exists
/// (e.g. a bare relative path with no existing parent).
fn canonicalize_from_nearest_ancestor(path: &std::path::Path) -> std::path::PathBuf {
    let mut ancestor = path;
    let mut trailing: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for part in trailing.iter().rev() {
                    resolved.push(part);
                }
                return resolved;
            }
            Err(_) => match ancestor.parent() {
                Some(parent) => {
                    if let Some(name) = ancestor.file_name() {
                        trailing.push(name);
                    }
                    ancestor = parent;
                }
                None => return path.to_path_buf(),
            },
        }
    }
}

/// Returns true if `path` is within `base` after canonicalization.
/// Handles `..` traversal attempts safely.
fn is_path_within(path: &std::path::Path, base: &std::path::Path) -> bool {
    // First, try canonicalization — this resolves symlinks, so a symlink inside
    // the project root pointing outside it cannot bypass the check.
    //
    // `path` frequently doesn't exist yet (a candidate file the caller is
    // about to check before it's written, or a path under a directory that
    // was never created — e.g. no `public/` dir in the project root), so
    // `canonicalize(path)` fails while `canonicalize(base)` (the project
    // root, which always exists) succeeds. Comparing an un-resolved `path`
    // against a resolved `base` then spuriously fails whenever `base` sits
    // behind a symlink the OS resolves transparently — macOS's
    // `/var` -> `/private/var` temp-dir symlink, or Windows' `\\?\`
    // extended-length prefix on canonicalized paths — even though `path`
    // and `base` describe the same real location. Resolve `path` from its
    // nearest existing ancestor instead, so both sides go through the same
    // symlink/prefix resolution.
    let canonical_base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let canonical_path = canonicalize_from_nearest_ancestor(path);

    if canonical_path.starts_with(&canonical_base) {
        return true;
    }

    // Fall back to lexical normalization for paths that don't exist yet
    // (canonicalize fails on non-existent paths)
    let mut normalized = std::path::PathBuf::new();
    for component in canonical_path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.starts_with(&canonical_base)
}

/// A TLS listener that wraps a TCP listener with tokio-rustls
struct TlsListener {
    listener: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, addr)) => match self.acceptor.accept(stream).await {
                    Ok(tls_stream) => return (tls_stream, addr),
                    Err(e) => {
                        tracing::warn!("TLS accept error: {}", e);
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!("TCP accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// A Unix domain socket listener for the dev server (Unix platforms only).
/// Mirrors `TlsListener` so `axum::serve` can accept connections on a
/// `tokio::net::UnixListener`.
#[cfg(unix)]
struct UnixSocketListener {
    listener: tokio::net::UnixListener,
}

#[cfg(unix)]
impl axum::serve::Listener for UnixSocketListener {
    type Io = tokio::net::UnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok(pair) => return pair,
                Err(e) => {
                    tracing::warn!("Unix socket accept error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// Start the dev server on a Unix domain socket instead of TCP (Unix only).
/// Removes a stale socket file before binding and cleans it up on shutdown.
#[cfg(unix)]
async fn start_unix_server(app: axum::Router, socket_path: &str) -> Result<()> {
    // Remove a stale socket file left behind by a previous run, if any
    if std::path::Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    tracing::info!("Dev server listening on unix://{}", socket_path);
    let shutdown = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
        tracing::info!("Shutdown signal received, draining connections...");
    };
    axum::serve(UnixSocketListener { listener }, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    // Clean up the socket file on shutdown
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

/// Shared state for the dev server, held behind an `Arc` and passed to
/// every axum route handler.
///
/// All mutable fields are behind `RwLock` so handlers can be serviced
/// concurrently on the multi-threaded tokio runtime.
pub struct DevServerState {
    /// The build engine used to transform modules on demand.
    pub engine: RwLock<BuildEngine>,
    /// The resolved PledgePack configuration for this server.
    pub config: Arc<PledgeConfig>,
    /// Sender side of the HMR broadcast channel; file watcher events are
    /// pushed here and forwarded to all connected browser clients.
    pub hmr_tx: mpsc::UnboundedSender<HmrUpdate>,
    /// Per-client senders for each connected HMR WebSocket.
    pub hmr_clients: RwLock<Vec<mpsc::UnboundedSender<HmrUpdate>>>,
    /// Import graph: module path → set of modules that import it (dependents)
    pub import_graph: RwLock<std::collections::HashMap<String, Vec<String>>>,
    /// Lazy-initialized transform pipeline (cold boot optimization)
    pub lazy_pipeline: RwLock<lazy_pipeline::LazyPipeline>,
    /// Module cache: path → last transformed output (for HMR diff computation)
    pub module_cache: RwLock<std::collections::HashMap<String, String>>,
    /// Import patterns per module: path → sorted import specifiers (for on-demand optimization)
    pub import_patterns: RwLock<std::collections::HashMap<String, Vec<String>>>,
    /// Middleware chain for request processing
    pub middleware_chain: RwLock<Vec<middleware::MiddlewareFn>>,
    /// Multi-entry HTML files: entry name → HTML content
    pub entries: RwLock<Vec<EntryConfig>>,
    /// Per-module plugin hooks (`resolveId` for `/@id/` virtual modules,
    /// `load`, chained `transform`) driven through the same
    /// [`pledgepack_core::plugin_hooks::PluginHooks`] trait as production
    /// builds. `None` when no plugin defines any of those hooks.
    pub plugin_hooks: Option<Arc<PluginHookService>>,
    /// Pre-bundled dependencies: bare specifier → served URL
    /// (`/node_modules/.pledge-deps/<file>.js`), produced by
    /// [`pledgepack_core::dep_bundler::DepBundler::pre_bundle`] at server
    /// boot — Vite-style dep pre-bundling so CJS deps are served locally
    /// instead of via a CDN and bare imports get stable dep URLs.
    pub prebundled_deps: std::sync::Arc<std::collections::HashMap<String, String>>,
}

/// Run dependency pre-bundling for dev-server startup: scans the project's
/// bare imports, resolves them through `pledgepack-resolver`, wraps CJS deps
/// in an ESM interop module and emits ESM re-export shims, all under
/// `node_modules/.pledge-deps/`. Returns specifier → URL for rewriting;
/// a pre-bundle failure is non-fatal (the import map still resolves deps).
fn prebundle_dependencies(config: &PledgeConfig) -> std::collections::HashMap<String, String> {
    if !config.root.join("node_modules").is_dir() {
        return std::collections::HashMap::new();
    }
    let mut bundler = pledgepack_core::dep_bundler::DepBundler::new();
    match bundler.pre_bundle(config) {
        Ok(deps) => {
            let map: std::collections::HashMap<String, String> = deps
                .iter()
                .map(|d| {
                    (
                        d.specifier.clone(),
                        pledgepack_core::dep_bundler::DepBundler::dep_url(&d.specifier),
                    )
                })
                .collect();
            if !map.is_empty() {
                info!("Pre-bundled {} dependencies into .pledge-deps", map.len());
            }
            map
        }
        Err(e) => {
            warn!("Dependency pre-bundling failed, serving deps per-file: {e}");
            std::collections::HashMap::new()
        }
    }
}

/// Configuration for a multi-entry dev server
#[derive(Debug, Clone)]
pub struct EntryConfig {
    /// Entry name (e.g., "index", "admin", "mobile")
    pub name: String,
    /// HTML file path relative to root
    pub html_file: String,
    /// JS entry module path
    pub entry_module: String,
}

/// An HMR update message sent to browser clients over the
/// `/__pledge_hmr` WebSocket.
///
/// Serialized as JSON; optional fields are omitted when absent so the
/// payload stays small. The `update_type` discriminant (e.g.
/// `"js-update"`, `"css-update"`, `"error"`, `"full-reload"`) tells the
/// client runtime how to apply the update.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HmrUpdate {
    /// Update kind discriminator (serialized as `"type"`).
    #[serde(rename = "type")]
    pub update_type: String,
    /// Project-relative path of the module that changed.
    pub path: String,
    /// Human-readable message (used for error overlays).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// File where an error originated, if different from `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Replacement CSS payload for `css-update` messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub css: Option<String>,
    /// Error stack trace for the overlay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// 1-based line number of an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column number of an error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    /// Modules that depend on the changed module (HMR boundary walk).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<String>,
    /// When true, instructs the client to reload the whole page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_reload: Option<bool>,
    /// Partial update: line-level diff for HMR (feature 10)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<hmr_diff::LineDiff>,
    /// Full module code for fallback when diff can't be applied
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_code: Option<String>,
    /// CSS Modules class name mappings (original → scoped) for HMR remapping
    #[serde(skip_serializing_if = "Option::is_none", rename = "moduleMap")]
    pub module_map: Option<serde_json::Value>,
}

/// Install the ring crypto provider once for rustls/reqwest.
/// Required because we use rustls-no-provider feature for cross-compilation.
fn ensure_crypto_provider() {
    use std::sync::OnceLock;
    static PROVIDER: OnceLock<()> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build the dev server router as a standalone app (for testing and embedding).
/// Returns a `Router` with all core routes and state configured.
///
/// # Panics
/// Panics if `dev_server.middleware` contains an entry that cannot be
/// executed; use [`try_create_app`] for the non-panicking variant.
pub fn create_app(config: PledgeConfig) -> Router {
    try_create_app(config).expect("invalid dev_server.middleware configuration")
}

/// Like [`create_app`], but returns an error instead of panicking when
/// `dev_server.middleware` contains an unsupported entry.
pub fn try_create_app(config: PledgeConfig) -> Result<Router> {
    try_create_app_with_plugin_hooks(config, None)
}

/// [`try_create_app`] with per-module plugin hooks attached (see
/// [`DevServerState::plugin_hooks`] and [`PluginHookService::spawn`]).
pub fn try_create_app_with_plugin_hooks(
    config: PledgeConfig,
    plugin_hooks: Option<Arc<PluginHookService>>,
) -> Result<Router> {
    let (hmr_tx, _hmr_rx) = mpsc::unbounded_channel::<HmrUpdate>();

    let engine = BuildEngine::new(Arc::new(config.clone()));

    // Unsupported middleware entries are an error, never silently dropped.
    let middleware_fns = middleware::build_chain(&config.dev_server.middleware)?;
    let extra_headers = middleware::response_headers(&middleware_fns);

    let entries = detect_entries(&config);
    let prebundled_deps = prebundle_dependencies(&config);

    let state = Arc::new(DevServerState {
        engine: RwLock::new(engine),
        config: config.clone().into(),
        hmr_tx,
        hmr_clients: RwLock::new(Vec::new()),
        import_graph: RwLock::new(std::collections::HashMap::new()),
        lazy_pipeline: RwLock::new(lazy_pipeline::LazyPipeline::new()),
        module_cache: RwLock::new(std::collections::HashMap::new()),
        import_patterns: RwLock::new(std::collections::HashMap::new()),
        middleware_chain: RwLock::new(middleware_fns),
        entries: RwLock::new(entries),
        plugin_hooks,
        prebundled_deps: std::sync::Arc::new(prebundled_deps),
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/__pledge_hmr", get(hmr_websocket_handler))
        .route("/__pledge_error", get(error_overlay_handler))
        .route("/__pledge_router", get(router_handler))
        .route("/__pledge_entry", get(entry_module_handler))
        .route("/__pledge_shell", get(shell_preview_handler))
        .route("/@fs/{*path}", get(virtual_fs_handler))
        .route("/@id/{*path}", get(virtual_id_handler))
        .route("/__pledge_public/{*path}", get(public_dir_handler))
        .route("/{*path}", get(app_route_handler))
        .with_state(state);
    let app = apply_response_headers(app, extra_headers);
    Ok(apply_origin_guard(app, &config))
}

/// Attach the response headers configured via `dev_server.middleware`
/// (`headers` entries) to every response.
fn apply_response_headers(app: Router, headers: Vec<(HeaderName, HeaderValue)>) -> Router {
    if headers.is_empty() {
        return app;
    }
    let headers = Arc::new(headers);
    app.layer(axum::middleware::from_fn(
        move |req, next: axum::middleware::Next| {
            let headers = headers.clone();
            async move {
                let mut resp = next.run(req).await;
                for (k, v) in headers.iter() {
                    resp.headers_mut().insert(k.clone(), v.clone());
                }
                resp
            }
        },
    ))
}

/// Origin/Host validation (cross-site WebSocket hijacking and DNS-rebinding
/// protection) for every route — see `origin_guard.rs`.
fn apply_origin_guard(app: Router, config: &PledgeConfig) -> Router {
    let policy = Arc::new(origin_guard::OriginPolicy::new(
        &config.dev_server.host,
        matches!(
            config.dev_server.cors,
            pledgepack_core::config::DevServerCors::Any
        ),
    ));
    app.layer(axum::middleware::from_fn(move |req, next| {
        let policy = policy.clone();
        async move { origin_guard::enforce(policy, req, next).await }
    }))
}

/// Start the dev server and run until shutdown (Ctrl+C).
///
/// Builds the axum router, starts the native file watcher for HMR when
/// `config.dev_server.hmr` is enabled, and binds on
/// `config.dev_server.host`/`config.dev_server.port`. Supports HTTPS via
/// `config.https` and Unix domain sockets via `config.dev_server.unix_socket`
/// (Unix platforms only).
///
/// The `engine` is used for on-demand module transforms. This function
/// only returns when the server shuts down or fails to bind.
pub async fn serve(engine: BuildEngine, config: &PledgeConfig) -> Result<()> {
    ensure_crypto_provider();
    let start = std::time::Instant::now();
    let port = config.dev_server.port;
    let host = config.dev_server.host.clone();

    // Non-loopback exposure: warn loudly, and require an access token unless
    // the user explicitly opted out. See PRODUCTION-READINESS-100.md goals
    // 17-18 — binding to e.g. `0.0.0.0` for LAN device testing previously
    // gave zero indication that it also exposes `/@fs/*` (arbitrary
    // read-within-project-root) to everyone on the network.
    let is_loopback = pledgepack_core::config::is_loopback_host(&host);
    let access_token: Option<String> = match &config.dev_server.access_token {
        Some(t) if t.is_empty() => None, // explicit opt-out
        Some(t) => Some(t.clone()),
        None if !is_loopback => Some(pledgepack_core::security::generate_random_token(16)),
        None => None,
    };
    // envPrefix floor: a "" / "*" prefix would expose every environment
    // variable to transformed modules. `pledge build` refuses outright; in
    // dev we warn loudly — the served code still leaks, but killing the dev
    // server over a config warning would be worse DX.
    if config
        .env_prefix
        .iter()
        .any(|p| p.is_empty() || p == "*")
    {
        eprintln!(
            "  \x1b[31m⚠ envPrefix contains an empty/\"*\" entry — ALL environment variables,\x1b[0m"
        );
        eprintln!(
            "    including .env secrets, will be inlined into served modules. `pledge build`"
        );
        eprintln!("    refuses this configuration; fix envPrefix before deploying.");
        eprintln!();
    }

    if !is_loopback {
        eprintln!();
        eprintln!(
            "  \x1b[33m⚠ pledgepack dev is bound to a non-loopback address ({}).\x1b[0m",
            host
        );
        eprintln!(
            "    This exposes your project's source files (via /@fs/*) to anyone who can reach"
        );
        eprintln!("    this host and port — e.g. others on the same network.");
        if let Some(ref token) = access_token {
            eprintln!(
                "    An access token is required: connect with ?token={} (or the X-Pledge-Token header).",
                token
            );
        } else {
            eprintln!(
                "    \x1b[31mAccess-token protection is disabled (dev_server.access_token set to \"\").\x1b[0m"
            );
        }
        eprintln!();
    }

    let (hmr_tx, hmr_rx) = mpsc::unbounded_channel::<HmrUpdate>();

    // Start native file watcher if HMR is enabled
    if config.dev_server.hmr {
        let watch_root = config.root.clone();
        let tx = hmr_tx.clone();
        let server_entry = config.server_entry.clone();
        let watcher_config = config.clone();
        // A dedicated OS thread, NOT `tokio::spawn`: the watcher loop blocks
        // on a std channel forever, which starves a current-thread runtime
        // (the server never accepts) and pins a worker on a multi-thread one.
        // The plugin host is built inside the thread so the non-`Send` QuickJS
        // host never crosses threads.
        let spawned = std::thread::Builder::new()
            .name("pledge-hmr-watcher".into())
            .spawn(move || {
                start_native_file_watcher(watch_root, tx, server_entry, &watcher_config);
            });
        if let Err(e) = spawned {
            warn!("could not start the file watcher thread: {e}");
        }
    }

    // Build middleware chain from config. Entries that cannot be executed are
    // a hard error (before anything is started), never silently ignored.
    let middleware_fns = middleware::build_chain(&config.dev_server.middleware)?;
    let extra_headers = middleware::response_headers(&middleware_fns);
    if !middleware_fns.is_empty() {
        info!(
            "Middleware chain: {} functions registered",
            middleware_fns.len()
        );
    }

    // Detect multi-entry HTML files
    // Per-module plugin hooks live on their own thread (QuickJS is `!Send`);
    // `configureServer` is NOT re-run for this second host instance.
    let serve_plugin_hooks = {
        let cfg = config.clone();
        PluginHookService::spawn(move || {
            let host = load_dev_plugin_host_with(&cfg, false)?;
            host.plugins()
                .iter()
                .any(|p| p.has_resolve_id || p.has_load || p.has_transform)
                .then(|| Box::new(host) as Box<dyn pledgepack_core::plugin_hooks::PluginHooks>)
        })
    };
    if serve_plugin_hooks.is_some() {
        info!("Plugin hooks (resolveId/load/transform) active for dev module serving");
    }

    let entries = detect_entries(config);
    if entries.len() > 1 {
        info!("Multi-entry dev server: {} entries detected", entries.len());
        for entry in &entries {
            info!(
                "  Entry '{}': {} → {}",
                entry.name, entry.html_file, entry.entry_module
            );
        }
    }

    // Vite-style dependency pre-bundling at server boot: bare imports are
    // resolved once, CJS deps get an ESM interop wrapper, and all bundled
    // deps are served from node_modules/.pledge-deps/ instead of being
    // re-resolved/transformed per request (or fetched from a CDN).
    let prebundled_deps = prebundle_dependencies(config);

    let state = Arc::new(DevServerState {
        engine: RwLock::new(engine),
        config: config.clone().into(),
        hmr_tx,
        hmr_clients: RwLock::new(Vec::new()),
        import_graph: RwLock::new(std::collections::HashMap::new()),
        lazy_pipeline: RwLock::new(lazy_pipeline::LazyPipeline::new()),
        module_cache: RwLock::new(std::collections::HashMap::new()),
        import_patterns: RwLock::new(std::collections::HashMap::new()),
        middleware_chain: RwLock::new(middleware_fns),
        entries: RwLock::new(entries),
        plugin_hooks: serve_plugin_hooks,
        prebundled_deps: std::sync::Arc::new(prebundled_deps),
    });

    // Spawn HMR broadcast task
    let hmr_state = state.clone();
    tokio::spawn(async move {
        hmr_broadcast_loop(hmr_state, hmr_rx).await;
    });

    // Build router with all stateful routes first
    let mut app = Router::new()
        .route("/", get(index_handler))
        .route("/__pledge_hmr", get(hmr_websocket_handler))
        .route("/__pledge_error", get(error_overlay_handler))
        .route("/__pledge_router", get(router_handler))
        .route("/__pledge_entry", get(entry_module_handler))
        .route("/__pledge_shell", get(shell_preview_handler))
        .route("/@fs/{*path}", get(virtual_fs_handler))
        .route("/@id/{*path}", get(virtual_id_handler))
        .route("/__pledge_public/{*path}", get(public_dir_handler));

    // Add multi-entry routes (e.g., /admin, /mobile) if configured
    {
        let entries = state.entries.read().await;
        for entry in entries.iter() {
            if entry.name != "index" {
                let entry_path = format!("/{}", entry.name);
                app = app.route(&entry_path, get(entry_index_handler));
                info!("Multi-entry route: {} → {}", entry_path, entry.html_file);
            }
        }
    }

    // Add catch-all route last
    let app = app
        .route("/{*path}", get(app_route_handler))
        .with_state(state.clone());

    // Proxy routes are merged BEFORE the middleware layers below so they sit
    // behind the same access-token gate, rate limit, and security headers as
    // every other route (merging them afterwards left them unauthenticated).
    let app = add_proxy_routes(app, &config.proxy);

    // Apply HTTP middleware: compression, body limits, security headers, and CORS
    // (feature 12: WebSocket per-message-deflate (RFC 7692) is NOT yet enabled —
    // axum's built-in WebSocketUpgrade does not expose an API to negotiate the
    // `permessage-deflate` extension during the handshake. This layer only
    // handles HTTP response compression (gzip/deflate/br).
    // Known limitation: enabling it would need a custom WebSocket upgrade layer.)
    // CORS: default to same-origin only (`CorsLayer::new()` with no
    // `allow_origin` emits no `Access-Control-*` headers at all, so a
    // browser's own same-origin policy applies). `DevServerCors::Any` opts
    // back into the previous unconditional wildcard behavior. See goal 16 —
    // this previously applied `Access-Control-Allow-Origin: *` to every
    // route unconditionally, including the raw-filesystem `/@fs/*` handler.
    let cors_layer = match config.dev_server.cors {
        pledgepack_core::config::DevServerCors::Any => tower_http::cors::CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any),
        pledgepack_core::config::DevServerCors::SameOrigin => tower_http::cors::CorsLayer::new(),
    };

    // CSP: wires the existing (previously build-report-only) CspGenerator
    // into actual dev-server response headers (goal 19). Relaxed relative to
    // a production CSP — `'unsafe-inline'`/`'unsafe-eval'` are required for
    // the injected HMR client `<script>` and for HMR's dynamic
    // module-replacement eval — appropriate for a local dev tool, not
    // intended as the policy a production build should ship.
    let dev_csp = {
        let mut csp = pledgepack_core::security::CspGenerator::new();
        csp.add_script_src("'unsafe-inline'");
        csp.add_script_src("'unsafe-eval'");
        csp.add_style_src("'unsafe-inline'");
        csp.generate()
    };
    let csp_header_value = HeaderValue::from_str(&dev_csp)
        .unwrap_or_else(|_| HeaderValue::from_static("default-src 'self'"));

    let mut app = app
        .layer(
            tower_http::compression::CompressionLayer::new()
                .gzip(true)
                .br(true)
                .quality(tower_http::CompressionLevel::Fastest),
        )
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            10 * 1024 * 1024,
        ))
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                header::X_FRAME_OPTIONS,
                HeaderValue::from_static("DENY"),
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                HeaderName::from_static("referrer-policy"),
                HeaderValue::from_static("strict-origin-when-cross-origin"),
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                header::CONTENT_SECURITY_POLICY,
                csp_header_value,
            ),
        )
        .layer(
            tower_http::set_header::SetResponseHeaderLayer::if_not_present(
                HeaderName::from_static("x-pledgepack-schema-version"),
                // PRODUCTION-READINESS-100.md goal 81: stamped on every
                // response (not just `/__pledge_router`) so any consumer can
                // check compatibility without depending on a specific route's
                // response shape. `/__pledge_router` itself returns a
                // JavaScript module, not JSON, so a body-embedded version
                // field wasn't a good fit there — a header works regardless of
                // content type. Computed from the same
                // `pledgepack_core::PLEDGESTACK_MANIFEST_SCHEMA_VERSION`
                // constant `RouteManifest::SCHEMA_VERSION` re-exports, so the
                // two surfaces can't drift apart.
                HeaderValue::from_str(
                    &pledgepack_core::PLEDGESTACK_MANIFEST_SCHEMA_VERSION.to_string(),
                )
                .unwrap_or_else(|_| HeaderValue::from_static("0")),
            ),
        )
        .layer(cors_layer);

    // `dev_server.middleware` `headers` entries.
    app = apply_response_headers(app, extra_headers);

    // Access-token gate (goal 18): only added when a token is actually
    // required (explicitly configured, or auto-generated above because the
    // server is bound to a non-loopback address). Loopback binds with no
    // explicit token stay open, matching prior behavior for the common
    // (safe) case.
    if let Some(expected_token) = access_token.clone() {
        let expected_token: Arc<str> = Arc::from(expected_token.as_str());
        app = app.layer(axum::middleware::from_fn(move |req, next| {
            let expected_token = expected_token.clone();
            async move { require_access_token(expected_token, req, next).await }
        }));
    }

    // Origin/Host validation, added after the token gate so it runs first.
    app = apply_origin_guard(app, config);

    // Global rate limit: a generous cap (not per-client — `tower`'s
    // RateLimitLayer has no client-identity concept) against a runaway HMR
    // reconnect loop or a malicious local script hammering the server.
    // Applied outermost (added last, so it wraps everything including the
    // access-token check above — a token brute-force attempt is limited
    // too) via `tower::util::BoxCloneSyncService`-compatible layering.
    // `DEV_SERVER_RATE_LIMIT_PER_SEC` lets this be raised or disabled
    // (`0` = disabled) for large projects with many legitimately concurrent
    // requests, without needing a full config-schema field for what's meant
    // as an escape hatch, not a tuning knob. See
    // PRODUCTION-READINESS-100.md goal 74.
    let rate_limit_per_sec: u64 = std::env::var("DEV_SERVER_RATE_LIMIT_PER_SEC")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    if rate_limit_per_sec > 0 {
        // `RateLimit<S>`'s inner state (the leaky-bucket counter) can't
        // implement `Clone` in a way that shares that state across clones —
        // a naive Clone would give every cloned instance its own
        // independent counter, defeating the point. axum's `Router::layer`
        // requires the layered service to be `Clone`, so this composes
        // `BufferLayer` (moves the rate limiter onto a background worker
        // task, handing out a `Clone`-able channel handle instead) and
        // `RateLimitLayer` into ONE `tower::ServiceBuilder` stack applied
        // via a single `.layer()` call — applying them as two separate
        // `Router::layer()` calls doesn't work: axum erases each call's
        // result back to a boxed `Route` before the next layer sees it, so
        // `RateLimitLayer` ends up wrapping a fresh `Route` instead of the
        // already-buffered (and thus Clone) service. `HandleErrorLayer`
        // goes outermost in the stack (applied first, so it sees
        // everything inside) to convert `Buffer`'s `BoxError` (surfaced
        // only if the buffer's worker task itself dies) into a real
        // response — axum requires the whole layered service's `Error`
        // type to be `Infallible`, and `HandleErrorLayer` is what makes
        // that true here.
        let rate_limit_stack = tower::ServiceBuilder::new()
            .layer(axum::error_handling::HandleErrorLayer::new(
                |err: tower::BoxError| async move {
                    tracing::error!("dev-server rate limiter failed: {err}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "rate limiter unavailable",
                    )
                },
            ))
            .layer(tower::buffer::BufferLayer::new(1024))
            .layer(tower::limit::RateLimitLayer::new(
                rate_limit_per_sec,
                std::time::Duration::from_secs(1),
            ));
        app = app.layer(rate_limit_stack);
    }

    // Execute configureServer hooks from JS plugins — same trust policy as
    // `config.plugins` (deny unsigned by default, opt out via
    // plugin_security.require_signed = false).
    // With HMR enabled the host lives on the file-watcher thread (QuickJS
    // contexts are not `Send`) so `handleHotUpdate` can run there; otherwise
    // it is only needed for `configureServer` and is dropped right away.
    if !config.dev_server.hmr {
        let _ = load_dev_plugin_host(config);
    }

    let addr = format!("{}:{}", host, port);
    let elapsed_ms = start.elapsed().as_millis();

    // Auto-open browser if configured
    if config.dev_server.open {
        let protocol = if config.https.is_some() {
            "https"
        } else {
            "http"
        };
        let url = format!("{}://{}", protocol, addr);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            open_browser(&url);
        });
    }

    // Unix domain socket support (Unix platforms only): when
    // `dev_server.unix_socket` is configured, listen on the socket path
    // instead of a TCP host:port.
    #[cfg(unix)]
    if let Some(ref socket_path) = config.dev_server.unix_socket {
        println!("\n  \x1b[32mReady in {}ms\x1b[0m\n", elapsed_ms);
        return start_unix_server(app, socket_path).await;
    }
    #[cfg(not(unix))]
    if config.dev_server.unix_socket.is_some() {
        tracing::warn!(
            "dev_server.unix_socket is only supported on Unix platforms; falling back to TCP"
        );
    }

    // Bind every listener up-front so the banner reports what is really
    // listening (`localhost` => both 127.0.0.1 and ::1).
    let listeners = bind_listeners(&host, port).await?;
    let bound: Vec<std::net::SocketAddr> = listeners
        .iter()
        .filter_map(|l| l.local_addr().ok())
        .collect();
    let scheme = if config.https.is_some() {
        "https"
    } else {
        "http"
    };

    // HTTPS support
    if let Some(ref https_config) = config.https {
        let cert_path = &https_config.cert;
        let key_path = &https_config.key;

        if !cert_path.exists() || !key_path.exists() {
            info!("HTTPS enabled but cert/key not found — generating self-signed certificate...");
            generate_self_signed_cert(cert_path, key_path)?;
            info!(
                "Self-signed certificate generated at {}",
                pledgepack_core::display_path(cert_path)
            );
        }

        // Use tokio-rustls for TLS
        let cert = match std::fs::read(cert_path) {
            Ok(c) => c,
            Err(e) => anyhow::bail!("Failed to read cert: {}", e),
        };
        let key = match std::fs::read(key_path) {
            Ok(k) => k,
            Err(e) => anyhow::bail!("Failed to read key: {}", e),
        };

        // Parse cert and key using rustls-pki-types PemObject API
        use rustls_pki_types::pem::PemObject;
        let cert_chain = rustls_pki_types::CertificateDer::pem_slice_iter(&cert)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("Failed to parse cert: {}", e))?;
        let key_der = rustls_pki_types::PrivateKeyDer::from_pem_slice(&key)
            .map_err(|e| anyhow::anyhow!("Failed to parse key: {}", e))?;

        let mut tls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain.into_iter().collect(), key_der)
            .map_err(|e| anyhow::anyhow!("Failed to build TLS config: {}", e))?;
        tls_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        let tls_acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(tls_config));

        announce_listening(scheme, &host, &bound, elapsed_ms);

        // Serve with TLS using a custom Listener implementation
        let shutdown = shutdown_signal();
        let servers = listeners.into_iter().map(|listener| {
            let tls_listener = TlsListener {
                listener,
                acceptor: tls_acceptor.clone(),
            };
            let mut rx = shutdown.clone();
            let app = app.clone();
            async move {
                axum::serve(tls_listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.wait_for(|v| *v).await;
                    })
                    .await
            }
        });
        futures_util::future::try_join_all(servers).await?;
    } else {
        announce_listening(scheme, &host, &bound, elapsed_ms);
        let shutdown = shutdown_signal();
        let servers = listeners.into_iter().map(|listener| {
            let mut rx = shutdown.clone();
            let app = app.clone();
            async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.wait_for(|v| *v).await;
                    })
                    .await
            }
        });
        futures_util::future::try_join_all(servers).await?;
    }

    Ok(())
}

/// Bind the dev server's TCP listener(s) for `host:port`.
///
/// `localhost` is bound on BOTH loopback families (127.0.0.1 and ::1): binding
/// only whichever address the resolver returns first (::1 on Windows) makes
/// `http://127.0.0.1:<port>` refuse connections. IPv6 loopback is optional (it
/// may be disabled), but IPv4 must succeed. Any other host binds exactly the
/// address it names.
async fn bind_listeners(host: &str, port: u16) -> Result<Vec<tokio::net::TcpListener>> {
    use std::net::{Ipv4Addr, Ipv6Addr};
    if host.eq_ignore_ascii_case("localhost") {
        let v4 = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to bind 127.0.0.1:{port}: {e}"))?;
        // With port 0 the OS picks one: reuse it for the IPv6 twin.
        let v6_port = v4.local_addr().map(|a| a.port()).unwrap_or(port);
        let mut out = vec![v4];
        match tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, v6_port)).await {
            Ok(v6) => out.push(v6),
            Err(e) => tracing::debug!("IPv6 loopback [::1]:{v6_port} not bound: {e}"),
        }
        return Ok(out);
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let listener = tokio::net::TcpListener::bind((bare, port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind {host}:{port}: {e}"))?;
    Ok(vec![listener])
}

/// A `watch` receiver that flips to `true` on Ctrl+C, so several servers (one
/// per listener) shut down together.
fn shutdown_signal() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Shutdown signal received, draining connections...");
            let _ = tx.send(true);
        }
    });
    rx
}

/// Format an address as a URL (`[::1]` brackets for IPv6).
fn url_for(scheme: &str, ip: std::net::IpAddr, port: u16) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => format!("{scheme}://{v4}:{port}"),
        std::net::IpAddr::V6(v6) => format!("{scheme}://[{v6}]:{port}"),
    }
}

/// The LAN URLs to advertise: only for listeners actually bound to a
/// non-loopback address (`0.0.0.0`/`::` => this machine's LAN IP). Loopback
/// binds are not reachable from the network, so they yield nothing.
fn network_urls(
    scheme: &str,
    bound: &[std::net::SocketAddr],
    lan_ip: Option<std::net::IpAddr>,
) -> Vec<String> {
    let mut urls = Vec::new();
    for addr in bound {
        let ip = addr.ip();
        let url = if ip.is_unspecified() {
            lan_ip.map(|lan| url_for(scheme, lan, addr.port()))
        } else if !ip.is_loopback() {
            Some(url_for(scheme, ip, addr.port()))
        } else {
            None
        };
        if let Some(u) = url
            && !urls.contains(&u)
        {
            urls.push(u);
        }
    }
    urls
}

/// Print the "Ready" banner plus the addresses that are really listening.
fn announce_listening(scheme: &str, host: &str, bound: &[std::net::SocketAddr], elapsed_ms: u128) {
    println!("\n  \x1b[32mReady in {}ms\x1b[0m\n", elapsed_ms);
    for addr in bound {
        info!(
            "Dev server running at {}",
            url_for(scheme, addr.ip(), addr.port())
        );
    }
    if bound.is_empty() {
        info!("Dev server running on {host}");
    }
    for url in network_urls(scheme, bound, local_ip_address::local_ip().ok()) {
        info!("  → Network: {url}");
    }
}

/// Cookie name used to remember a validated dev-server access token (see
/// [`require_access_token`]) so the browser doesn't need to repeat
/// `?token=...` on every request — notably including the HMR WebSocket
/// handshake, which the injected client script opens with a fixed URL it
/// doesn't know to append a token to. Browsers attach cookies to a WS
/// handshake automatically (it's a plain HTTP GET with an Upgrade header),
/// so this covers that case with no client-script changes needed.
const ACCESS_TOKEN_COOKIE: &str = "pledge_token";

/// Length-independent-timing comparison so the access token can't be
/// recovered byte-by-byte from response latency.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

fn token_from_query(uri: &axum::http::Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "token").then(|| v.to_string())
    })
}

fn token_from_cookie(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == ACCESS_TOKEN_COOKIE).then(|| v.to_string())
    })
}

fn token_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-pledge-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Middleware gating every route behind `expected_token`, checked (in
/// order) against `?token=`, the `X-Pledge-Token` header, and a
/// `pledge_token` cookie. On success via query param, sets that cookie so
/// subsequent requests (including the HMR WebSocket handshake) don't need
/// to repeat it. See PRODUCTION-READINESS-100.md goal 18.
async fn require_access_token(
    expected_token: Arc<str>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let query_token = token_from_query(req.uri());
    let matches = |candidate: Option<String>| {
        candidate.is_some_and(|c| constant_time_eq(c.as_bytes(), expected_token.as_bytes()))
    };
    let query_ok = matches(query_token.clone());
    let authorized = query_ok
        || matches(token_from_header(req.headers()))
        || matches(token_from_cookie(req.headers()));

    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            "pledgepack dev: this server requires an access token because it is bound to a \
             non-loopback address.\nOpen the URL printed at startup (it includes ?token=...), \
             or pass the token via the X-Pledge-Token header.",
        )
            .into_response();
    }

    let mut response = next.run(req).await;
    // Only remember the token when the query token itself was the valid one
    // (previously any `?token=` value — even a wrong one, alongside a valid
    // header — caused the cookie to be issued).
    if query_ok
        && let Ok(cookie) = HeaderValue::from_str(&format!(
            "{ACCESS_TOKEN_COOKIE}={expected_token}; Path=/; HttpOnly; SameSite=Strict"
        ))
    {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

/// Serve the virtual router module for file-based routing in dev mode
async fn router_handler(State(state): State<Arc<DevServerState>>) -> Response {
    // Scan the app directory and generate the router module
    if let Some(app_dir) = state.config.resolve_app_dir() {
        match pledgepack_core::router::scan_app_dir(&state.config.root, &app_dir) {
            Ok(route_table) => {
                let router_module = route_table.generate_router_module();
                return (
                    [
                        (
                            header::CONTENT_TYPE,
                            "application/javascript; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    router_module,
                )
                    .into_response();
            }
            Err(e) => {
                let error_body = format!(
                    "console.error('[pledge] Router generation error: {}');\nexport function render() {{ return null; }}",
                    e
                );
                return (
                    [
                        (
                            header::CONTENT_TYPE,
                            "application/javascript; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    error_body,
                )
                    .into_response();
            }
        }
    }

    // No app directory — return a minimal router that renders nothing
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        "export function render() { return null; }".to_string(),
    )
        .into_response()
}

/// Serve the auto-generated entry module (replaces static entry.tsx)
async fn entry_module_handler() -> Response {
    let entry_code = shell_generator::generate_entry_module();
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        entry_code,
    )
        .into_response()
}

/// Shell preview endpoint — shows the generated HTML shell for debugging
async fn shell_preview_handler(State(state): State<Arc<DevServerState>>) -> Response {
    let (html_attrs, head_content) =
        match shell_generator::try_extract_shell_from_project(&state.config.root) {
            Some((attrs, head)) => (attrs, head),
            None => (
                "lang=\"en\"".to_string(),
                "<title>PledgeStack</title>".to_string(),
            ),
        };

    let import_map = generate_import_map(&state.config, &state.prebundled_deps);

    // Show the raw shell (without HMR script) for inspection
    let shell = shell_generator::generate_html_shell(
        &html_attrs,
        &head_content,
        "<!-- HMR script injected here -->",
        &import_map,
    );

    let escaped = shell
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Pledge Shell Preview</title>
<style>
  body {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #1a1a1a; color: #e0e0e0; padding: 2rem; }}
  h1 {{ color: #6c63ff; }}
  pre {{ background: #0d0d0d; border: 1px solid #333; border-radius: 8px; padding: 1.5rem; overflow: auto; font-size: 0.85rem; line-height: 1.6; }}
  .info {{ color: #888; margin-bottom: 1rem; }}
</style>
</head>
<body>
  <h1>Pledge Shell Preview</h1>
  <div class="info">This is the auto-generated HTML shell from layout.tsx. HMR script is shown as a comment.</div>
  <pre>{}</pre>
</body>
</html>"#,
        escaped
    );

    Html(html).into_response()
}

/// The browser-side HMR client: WebSocket reconnect with backoff, module/CSS/
/// framework hot-update handling, the error overlay, and the runtime-error
/// hooks. Single source of truth — this previously existed as three inline
/// copies (index, SPA-fallback, and multi-entry handlers) that had diverged:
/// only one had framework `.vue`/`.svelte` HMR and the full error overlay.
const HMR_CLIENT_TAG: &str = include_str!("hmr_client.html");

/// Insert the HMR client `<script>` block immediately before the first
/// `</body>`; when the document has no body close tag, append one.
fn inject_hmr_client(html: &mut String) {
    if html.contains("</body>") {
        *html = html.replacen("</body>", &format!("{}\n</body>", HMR_CLIENT_TAG), 1);
    } else {
        html.push_str(HMR_CLIENT_TAG);
        html.push_str("\n</body></html>");
    }
}

/// Serve the index.html shell
async fn index_handler(State(state): State<Arc<DevServerState>>) -> impl IntoResponse {
    // Convention: auto-detect project structure
    // Priority: app/ → src/app/ → src/ → root
    let (html_path, base_dir) = if let Some(entry) = &state.config.html_entry {
        (state.config.root.join(entry), None)
    } else {
        let base = state.config.resolve_base_dir();
        let html = match &base {
            Some(b) => state.config.root.join(b).join("index.html"),
            None => state.config.root.join("index.html"),
        };
        (html, base)
    };

    let mut html = match std::fs::read_to_string(&html_path) {
        Ok(content) => content,
        Err(_) => {
            // Fall back to root index.html if base_dir index.html doesn't exist
            let root_html = state.config.root.join("index.html");
            if root_html != html_path {
                if let Ok(content) = std::fs::read_to_string(&root_html) {
                    content
                } else {
                    // Auto-generate HTML shell from layout.tsx (no static index.html needed)
                    let (html_attrs, head_content) =
                        match shell_generator::try_extract_shell_from_project(&state.config.root) {
                            Some((attrs, head)) => (attrs, head),
                            None => (
                                "lang=\"en\"".to_string(),
                                "<title>PledgeStack</title>".to_string(),
                            ),
                        };
                    let import_map = generate_import_map(&state.config, &state.prebundled_deps);
                    shell_generator::generate_html_shell(
                        &html_attrs,
                        &head_content,
                        "",
                        &import_map,
                    )
                }
            } else {
                // Auto-generate HTML shell from layout.tsx (no static index.html needed)
                let (html_attrs, head_content) =
                    match shell_generator::try_extract_shell_from_project(&state.config.root) {
                        Some((attrs, head)) => (attrs, head),
                        None => (
                            "lang=\"en\"".to_string(),
                            "<title>PledgeStack</title>".to_string(),
                        ),
                    };
                let import_map = generate_import_map(&state.config, &state.prebundled_deps);
                shell_generator::generate_html_shell(&html_attrs, &head_content, "", &import_map)
            }
        }
    };

    // Inject HMR client script before </body>
    // Rewrite relative paths to absolute paths based on base_dir
    // e.g., ./index.tsx → /src/index.tsx, ./styles.css → /src/styles.css
    if let Some(base) = base_dir {
        let prefix = format!("/{}", base);
        // Rewrite src="./..." and href="./..." to absolute paths
        html = html.replace("src=\"./", &format!("src=\"{}/", prefix));
        html = html.replace("src='./", &format!("src='{}/", prefix));
        html = html.replace("href=\"./", &format!("href=\"{}/", prefix));
        html = html.replace("href='./", &format!("href='{}/", prefix));
    }

    // Inject import map for bare specifiers (react, react-dom, etc.)
    // Skip if HTML already contains an import map (e.g., from shell generator)
    if !html.contains("type=\"importmap\"") {
        let import_map = generate_import_map(&state.config, &state.prebundled_deps);
        if !import_map.is_empty() {
            let map_tag = format!("<script type=\"importmap\">\n{}\n</script>\n", import_map);
            if html.contains("</head>") {
                html = html.replace("</head>", &format!("{}\n</head>", map_tag));
            } else if html.contains("<body") {
                html = html.replace("<body", &format!("{}\n<body", map_tag));
            } else {
                html = format!("{}\n{}", map_tag, html);
            }
        }
    }

    inject_hmr_client(&mut html);

    Html(html)
}

/// Handle app-style routes: serve index.html for non-asset paths, fall back to module_handler for assets
async fn app_route_handler(
    State(state): State<Arc<DevServerState>>,
    Path(path): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
) -> Response {
    // If the path looks like a static asset (has a file extension), serve it as a module
    let has_extension = path
        .rsplit('/')
        .next()
        .map(|last| last.contains('.'))
        .unwrap_or(false);

    if has_extension {
        return module_handler(
            State(state),
            Path(path),
            wants_module(query.as_deref(), &headers),
        )
        .await;
    }

    // Non-asset path — serve the index.html shell for client-side routing
    // This enables both app-router and SPA-style routing
    let (html_path, base_dir) = if let Some(entry) = &state.config.html_entry {
        (state.config.root.join(entry), None)
    } else {
        let base = state.config.resolve_base_dir();
        let html = match &base {
            Some(b) => state.config.root.join(b).join("index.html"),
            None => state.config.root.join("index.html"),
        };
        (html, base)
    };

    let mut html = match std::fs::read_to_string(&html_path) {
        Ok(content) => content,
        Err(_) => {
            // Auto-generate HTML shell from layout.tsx (no static index.html needed)
            let (html_attrs, head_content) =
                match shell_generator::try_extract_shell_from_project(&state.config.root) {
                    Some((attrs, head)) => (attrs, head),
                    None => (
                        "lang=\"en\"".to_string(),
                        "<title>PledgeStack</title>".to_string(),
                    ),
                };
            let import_map = generate_import_map(&state.config, &state.prebundled_deps);
            shell_generator::generate_html_shell(&html_attrs, &head_content, "", &import_map)
        }
    };

    // Rewrite relative paths to absolute paths based on base_dir
    if let Some(base) = base_dir {
        let prefix = format!("/{}", base);
        html = html.replace("src=\"./", &format!("src=\"{}/", prefix));
        html = html.replace("src='./", &format!("src='{}/", prefix));
        html = html.replace("href=\"./", &format!("href=\"{}/", prefix));
        html = html.replace("href='./", &format!("href='{}/", prefix));
    }

    // Inject import map for bare specifiers (react, react-dom, etc.)
    // Skip if HTML already contains an import map (e.g., from shell generator)
    if !html.contains("type=\"importmap\"") {
        let import_map = generate_import_map(&state.config, &state.prebundled_deps);
        if !import_map.is_empty() {
            let map_tag = format!("<script type=\"importmap\">\n{}\n</script>\n", import_map);
            if html.contains("</head>") {
                html = html.replace("</head>", &format!("{}\n</head>", map_tag));
            } else if html.contains("<body") {
                html = html.replace("<body", &format!("{}\n<body", map_tag));
            } else {
                html = format!("{}\n{}", map_tag, html);
            }
        }
    }

    // Inject HMR client script before </body>
    inject_hmr_client(&mut html);

    Html(html).into_response()
}

/// Generate an import map for bare specifiers (react, vue, etc.)
/// This allows the browser to resolve bare imports without a bundler.
/// For CJS-only packages (no ESM), use esm.sh CDN.
/// For ESM packages, serve locally from node_modules.
fn generate_import_map(
    config: &PledgeConfig,
    prebundled_deps: &std::collections::HashMap<String, String>,
) -> String {
    let node_modules = config.root.join("node_modules");
    let mut imports = serde_json::Map::new();

    // Known CJS-only packages that need esm.sh CDN
    let cjs_only_packages = ["react", "react-dom", "scheduler"];

    if node_modules.is_dir()
        && let Ok(entries) = std::fs::read_dir(&node_modules)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if name.starts_with('@') {
                let scoped_dir = entry.path();
                if scoped_dir.is_dir()
                    && let Ok(scoped_entries) = std::fs::read_dir(&scoped_dir)
                {
                    for se in scoped_entries.flatten() {
                        let sub_name = se.file_name().to_string_lossy().to_string();
                        let pkg_name = format!("{}/{}", name, sub_name);
                        let pkg_json = scoped_dir.join(&sub_name).join("package.json");
                        if let Ok(content) = std::fs::read_to_string(&pkg_json)
                            && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
                        {
                            let has_esm = pkg.get("module").is_some()
                                || pkg.get("type").and_then(|v| v.as_str()) == Some("module");
                            if has_esm {
                                let entry_field = pkg
                                    .get("module")
                                    .or_else(|| pkg.get("main"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("index.js");
                                imports.insert(
                                    pkg_name,
                                    serde_json::Value::String(normalize_path_str(&format!(
                                        "/node_modules/{}/{}/{}",
                                        name, sub_name, entry_field
                                    ))),
                                );
                            } else {
                                // CJS-only: use esm.sh
                                imports.insert(
                                    pkg_name.clone(),
                                    serde_json::Value::String(format!(
                                        "https://esm.sh/{}",
                                        pkg_name
                                    )),
                                );
                            }
                        }
                    }
                }
                continue;
            }
            let pkg_json = node_modules.join(&name).join("package.json");
            if let Ok(content) = std::fs::read_to_string(&pkg_json)
                && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
            {
                let has_esm = pkg.get("module").is_some()
                    || pkg.get("type").and_then(|v| v.as_str()) == Some("module");

                // Check if exports has an "import" condition
                let has_esm_export = pkg
                    .get("exports")
                    .and_then(|e| e.as_object())
                    .and_then(|o| o.get("."))
                    .and_then(|d| d.as_object())
                    .map(|d| d.contains_key("import") || d.contains_key("browser"))
                    .unwrap_or(false);

                let is_cjs_only =
                    cjs_only_packages.contains(&name.as_str()) || (!has_esm && !has_esm_export);

                if is_cjs_only {
                    // CJS-only: use esm.sh CDN for all entry points
                    imports.insert(
                        name.clone(),
                        serde_json::Value::String(format!("https://esm.sh/{}", name)),
                    );

                    // Add exports map entries via esm.sh
                    if let Some(exports) = pkg.get("exports")
                        && let Some(obj) = exports.as_object()
                    {
                        for (export_key, _) in obj {
                            if export_key == "." {
                                continue;
                            }
                            let full_key =
                                format!("{}/{}", name, export_key.trim_start_matches("./"));
                            imports.insert(
                                full_key,
                                serde_json::Value::String(format!(
                                    "https://esm.sh/{}/{}",
                                    name,
                                    export_key.trim_start_matches("./")
                                )),
                            );
                        }
                    }
                } else {
                    // ESM package: serve locally
                    let entry_field = pkg
                        .get("module")
                        .or_else(|| pkg.get("main"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("index.js");
                    imports.insert(
                        name.clone(),
                        serde_json::Value::String(normalize_path_str(&format!(
                            "/node_modules/{}/{}",
                            name, entry_field
                        ))),
                    );

                    // Add exports map entries — resolved through the shared
                    // package-map matcher (conditions, arrays, nesting),
                    // not a hand-rolled condition pick.
                    if let Some(exports) = pkg.get("exports")
                        && let Some(obj) = exports.as_object()
                    {
                        let dev_conditions: Vec<String> =
                            ["browser", "module", "import", "default"]
                                .iter()
                                .map(|s| s.to_string())
                                .collect();
                        for export_key in obj.keys() {
                            if export_key == "." || export_key.contains('*') {
                                continue;
                            }
                            let resolved = pledgepack_resolver::package_map::resolve_exports_entry(
                                exports,
                                export_key,
                                &dev_conditions,
                            );
                            if let Some(resolved_path) = resolved {
                                let full_key =
                                    format!("{}/{}", name, export_key.trim_start_matches("./"));
                                imports.insert(
                                    full_key,
                                    serde_json::Value::String(normalize_path_str(&format!(
                                        "/node_modules/{}/{}",
                                        name,
                                        resolved_path.trim_start_matches("./")
                                    ))),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Pre-bundled deps win over every other mapping: CJS deps get their
    // .pledge-deps interop bundle instead of the esm.sh CDN fallback, and
    // ESM deps get the stable shim URL.
    for (spec, url) in prebundled_deps {
        imports.insert(spec.clone(), serde_json::Value::String(url.clone()));
    }

    // Build scopes for multi-version deduplication.
    // Scans nested node_modules for different versions of the same package
    // and creates scoped import map entries so modules depending on different
    // versions resolve correctly.
    let scopes = build_import_map_scopes(&node_modules, &imports);

    if scopes.is_empty() {
        serde_json::json!({ "imports": imports }).to_string()
    } else {
        serde_json::json!({ "imports": imports, "scopes": scopes }).to_string()
    }
}

/// Build scoped import map entries for packages with multiple versions.
/// When a package has different versions in nested node_modules, we create
/// a scope entry for each parent module path so the browser resolves the
/// correct version.
fn build_import_map_scopes(
    root_node_modules: &std::path::Path,
    _top_level_imports: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut scopes: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    let mut pkg_versions: HashMap<String, Vec<(String, String)>> = HashMap::new();

    // Walk all nested node_modules directories to find version conflicts
    fn scan_node_modules(
        dir: &std::path::Path,
        root: &std::path::Path,
        pkg_versions: &mut HashMap<String, Vec<(String, String)>>,
    ) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue;
                }
                if name.starts_with('@') {
                    let scoped_dir = entry.path();
                    if scoped_dir.is_dir()
                        && let Ok(scoped_entries) = std::fs::read_dir(&scoped_dir)
                    {
                        for se in scoped_entries.flatten() {
                            let sub_name = se.file_name().to_string_lossy().to_string();
                            let pkg_name = format!("{}/{}", name, sub_name);
                            let pkg_json = scoped_dir.join(&sub_name).join("package.json");
                            if let Ok(content) = std::fs::read_to_string(&pkg_json)
                                && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
                            {
                                let version = pkg
                                    .get("version")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("0.0.0");
                                let rel_path = dir
                                    .strip_prefix(root)
                                    .unwrap_or(dir)
                                    .to_string_lossy()
                                    .to_string();
                                pkg_versions
                                    .entry(pkg_name)
                                    .or_default()
                                    .push((version.to_string(), rel_path));
                            }
                        }
                    }
                    continue;
                }
                let pkg_json = dir.join(&name).join("package.json");
                if let Ok(content) = std::fs::read_to_string(&pkg_json)
                    && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
                {
                    let version = pkg
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0.0.0");
                    let rel_path = dir
                        .strip_prefix(root)
                        .unwrap_or(dir)
                        .to_string_lossy()
                        .to_string();
                    pkg_versions
                        .entry(name)
                        .or_default()
                        .push((version.to_string(), rel_path));
                }
            }
        }

        // Recurse into subdirectories that contain nested node_modules
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path
                        .file_name()
                        .map(|n| n != "node_modules")
                        .unwrap_or(true)
                {
                    let nested_nm = path.join("node_modules");
                    if nested_nm.is_dir() {
                        scan_node_modules(&nested_nm, root, pkg_versions);
                    }
                }
            }
        }
    }

    scan_node_modules(root_node_modules, root_node_modules, &mut pkg_versions);

    // For packages with multiple versions, create scope entries
    for (pkg_name, versions) in &pkg_versions {
        let unique_versions: HashSet<&String> = versions.iter().map(|(v, _)| v).collect();
        if unique_versions.len() <= 1 {
            continue;
        }

        // Group by version, pick the first path for each version
        let mut by_version: HashMap<&String, &String> = HashMap::new();
        for (version, path) in versions {
            by_version.entry(version).or_insert(path);
        }

        // For each version, create a scope that maps the package to the correct path
        for parent_path in by_version.values() {
            let scope_key = format!(
                "/node_modules/{}/",
                parent_path.trim_start_matches("node_modules/")
            );
            let scope_key = scope_key.replace("//", "/");

            let scope_entry = scopes
                .entry(scope_key.clone())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

            if let Some(scope_obj) = scope_entry.as_object_mut() {
                // Map this package to its nested version path
                let nested_path = format!(
                    "/node_modules/{}/{}/",
                    parent_path.trim_start_matches("node_modules/"),
                    pkg_name
                );
                let nested_path = nested_path.replace("//", "/");

                // Try to get the entry field from the nested package.json
                let nested_pkg_json = root_node_modules
                    .join(parent_path.trim_start_matches("node_modules/"))
                    .join(pkg_name)
                    .join("package.json");

                let entry_field = std::fs::read_to_string(&nested_pkg_json)
                    .ok()
                    .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                    .and_then(|p| {
                        p.get("module")
                            .or_else(|| p.get("main"))
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    })
                    .unwrap_or_else(|| "index.js".to_string());

                let full_path = normalize_path_str(&format!("{}{}", nested_path, entry_field));
                scope_obj.insert(pkg_name.clone(), serde_json::Value::String(full_path));
            }
        }
    }

    scopes
}

/// Error overlay endpoint — returns error info as JSON for programmatic access
async fn error_overlay_handler(State(_state): State<Arc<DevServerState>>) -> Response {
    // Return a simple page that can be used to display errors
    let html = r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>Pledge Error</title></head>
<body style="background:#1a1a1a;color:#ff4444;font-family:monospace;padding:2rem;">
<h1>&#9888; Pledge Build Error</h1>
<p>Check the console for details.</p>
</body>
</html>"#;
    Html(html).into_response()
}

/// JS module served in place of a module whose transform failed. The path and
/// message are JSON-encoded: raw interpolation into a `'...'` literal broke
/// (and allowed code injection) whenever the message contained a quote or
/// newline.
fn transform_error_module(path: &str, message: &str) -> String {
    let quoted_path = serde_json::to_string(path).unwrap_or_else(|_| "\"\"".to_string());
    let quoted_msg = serde_json::to_string(message).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "/* Pledge Transform Error */\nconsole.error('[pledge] Transform error in ' + {quoted_path} + ': ' + {quoted_msg});\nthrow new Error('Transform error: ' + {quoted_msg});"
    )
}

/// Map the `/@fs/<path>` wildcard to a filesystem path: POSIX absolute paths
/// arrive without their leading `/`, but Windows drive paths (`C:/proj/a.js`)
/// must be used as-is — prefixing `/` yields an invalid `/C:/...`.
fn fs_path_from_request(path: &str) -> std::path::PathBuf {
    let b = path.as_bytes();
    if (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || path.starts_with('/') {
        std::path::PathBuf::from(path)
    } else {
        std::path::PathBuf::from(format!("/{}", path))
    }
}

fn denied_response(d: fs_guard::Denied) -> Response {
    match d {
        fs_guard::Denied::NotFound => (StatusCode::NOT_FOUND, "Not found").into_response(),
        fs_guard::Denied::Forbidden => (StatusCode::FORBIDDEN, "Access denied").into_response(),
    }
}

/// Whether a request is an ES-module import (as opposed to a plain fetch):
/// an explicit `?import` marker, or a browser module-script fetch
/// (`Sec-Fetch-Dest: script`). Only matters for JSON files: they are served
/// wrapped as a JS module for imports and as `application/json` otherwise.
fn wants_module(query: Option<&str>, headers: &HeaderMap) -> bool {
    let has_import_flag = query.is_some_and(|q| {
        q.split('&')
            .any(|kv| kv == "import" || kv.starts_with("import="))
    });
    has_import_flag
        || headers
            .get("sec-fetch-dest")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("script"))
}

/// Serve a transformed module on-demand
async fn module_handler(
    State(state): State<Arc<DevServerState>>,
    Path(path): Path<String>,
    as_module: bool,
) -> Response {
    // First, try serving from the configured public directory (static assets)
    let public_dir = &state.config.dev_server.public_dir;
    let public_path = state.config.root.join(public_dir).join(&path);

    // Security: prevent path traversal via `../` in the request path
    if !is_path_within(&public_path, &state.config.root) {
        return (StatusCode::FORBIDDEN, "Path traversal denied").into_response();
    }

    if public_path.is_file() {
        match fs_guard::resolve_servable(&state.config.root, &path, &public_path) {
            Ok(canonical) => {
                if let Ok(content) = tokio::fs::read(&canonical).await {
                    if content.len() > MAX_RESPONSE_SIZE {
                        return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large")
                            .into_response();
                    }
                    let content_type = guess_content_type(&path);
                    return ([(header::CONTENT_TYPE, content_type)], content).into_response();
                }
            }
            Err(d) => return denied_response(d),
        }
    }

    let full_path = state.config.root.join(&path);

    // Security: prevent path traversal via `../` in the request path
    if !is_path_within(&full_path, &state.config.root) {
        return (StatusCode::FORBIDDEN, "Path traversal denied").into_response();
    }

    // If the exact file doesn't exist, try alternative extensions
    // (e.g., /src/utils.js → /src/utils.ts, /src/index.js → /src/index.tsx)
    let full_path = if full_path.exists() {
        full_path
    } else {
        // Try replacing .js extension with source extensions
        let stem = full_path.with_extension("");
        let mut found = None;
        for ext in &[
            "tsx", "ts", "jsx", "js", "mjs", "css", "json", "vue", "svelte",
        ] {
            let candidate = stem.with_extension(ext);
            if candidate.exists() {
                found = Some(candidate);
                break;
            }
        }
        match found {
            Some(p) => p,
            None => return (StatusCode::NOT_FOUND, "Module not found").into_response(),
        }
    };

    // Deny-list / allowed-roots / extension allowlist on the final resolved
    // file (after the extension fallback above, and after symlink resolution).
    // (The canonical path is only used for validation; the un-prefixed path is
    // kept for reading so downstream tooling never sees `\\?\` paths.)
    let is_json = full_path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let guarded = if is_json && as_module {
        fs_guard::resolve_servable_module(&state.config.root, &path, &full_path)
    } else {
        fs_guard::resolve_servable(&state.config.root, &path, &full_path)
    };
    if let Err(d) = guarded {
        return denied_response(d);
    }

    // A JSON file fetched as data (not imported as a module) is served as
    // JSON, not wrapped into an ES module with the HMR polyfill.
    if is_json && !as_module {
        return match tokio::fs::read(&full_path).await {
            Ok(content) if content.len() > MAX_RESPONSE_SIZE => {
                (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response()
            }
            Ok(content) => (
                [
                    (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                content,
            )
                .into_response(),
            Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read").into_response(),
        };
    }

    // Read source via Zig I/O
    let source = match pledgepack_native_sys::read_file(full_path.to_str().unwrap_or("")) {
        Ok(content) => content,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read").into_response(),
    };

    if source.len() > MAX_RESPONSE_SIZE {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
    }

    let source_str = String::from_utf8_lossy(&source).to_string();

    // Plugin `load` (may replace the file's content) then the chained
    // `transform` hooks, exactly as the production build applies them - before
    // the built-in transform. `node_modules` is skipped (dev serves
    // dependencies untouched, and running JS hooks per dependency file would
    // dominate cold start). The plugin map traces the result back to the file.
    let mut plugin_map: Option<String> = None;
    let source_str = match state.plugin_hooks.as_ref() {
        Some(hooks) if !path.starts_with("node_modules/") => {
            let id = normalize_path(&full_path);
            match run_plugin_module_hooks(hooks, &id, source_str).await {
                Ok((code, map)) => {
                    plugin_map = map;
                    code
                }
                Err(e) => {
                    return plugin_hook_error_response(
                        &state,
                        &path,
                        full_path.to_str().unwrap_or(""),
                        &e,
                    );
                }
            }
        }
        _ => source_str,
    };

    // CJS → ESM conversion for node_modules files
    // Browser can't use require()/module.exports, so wrap them in ESM
    let (source_str, skip_transform) = if path.starts_with("node_modules/") {
        let is_cjs = source_str.contains("module.exports")
            || source_str.contains("require(")
            || source_str.contains("exports.");

        if is_cjs {
            let specifier = path.strip_prefix("node_modules/").unwrap_or(&path);
            let rewritten = rewrite_cjs_requires(&source_str, &path);
            let wrapped =
                pledgepack_core::dep_bundler::DepBundler::cjs_to_esm_wrapper(specifier, &rewritten);
            (wrapped, true)
        } else {
            (source_str, false)
        }
    } else {
        (source_str, false)
    };

    // For CJS-wrapped node_modules, skip Oxc transform and serve directly
    if skip_transform {
        let rewritten = rewrite_imports(
            &source_str,
            &path,
            &state.config.resolve_alias,
            &state.prebundled_deps,
            &state.config.root,
        );
        return serve_js_module(&path, &rewritten, None, &state).await;
    }

    // Determine module kind from extension
    let ext_str = full_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e))
        .unwrap_or_default();
    let kind = ModuleKind::from_extension(&ext_str);

    // Transform using Oxc (JSX → JS, TS type stripping) or Lightning CSS
    // Uses lazy pipeline initialization (feature 11: cold boot optimization)
    // The transform pipeline (Oxc, Lightning CSS) is only loaded on first request
    let file_path = full_path.to_str().unwrap_or("");
    let transform_output = {
        let mut lazy_pipeline = state.lazy_pipeline.write().await;
        lazy_pipeline.ensure_initialized();
        match pledge_transform::transform(&source_str, kind, file_path, false, &state.config) {
            Ok(output) => output,
            Err(e) => {
                // Send error to all HMR clients via WebSocket
                let clean = format!("{:#}", e);
                let location = pledge_diagnostics::parse_location(&clean);
                let error_update = HmrUpdate {
                    update_type: "error".to_string(),
                    path: path.clone(),
                    message: Some(clean.clone()),
                    file: Some(file_path.to_string()),
                    css: None,
                    stack: Some(clean),
                    line: location.map(|l| l.0 as u32),
                    column: location.map(|l| l.1 as u32),
                    deps: Vec::new(),
                    full_reload: None,
                    diff: None,
                    full_code: None,
                    module_map: None,
                };
                let _ = state.hmr_tx.send(error_update);
                // Also return an error response with proper content type
                let error_body = transform_error_module(&path, &e.to_string());
                return (
                    [
                        (
                            header::CONTENT_TYPE,
                            "application/javascript; charset=utf-8",
                        ),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    error_body,
                )
                    .into_response();
            }
        }
    };

    // Track import patterns for on-demand dependency optimization (feature 15)
    {
        let import_patterns = state.import_patterns.read().await;
        if !import_patterns.contains_key(&path) {
            drop(import_patterns);
            let mut import_patterns = state.import_patterns.write().await;
            import_patterns.insert(path.clone(), extract_imports(&source_str));
        }
    }

    // Cache the transformed module for HMR diff computation (feature 10)
    // Bounded to MAX_MODULE_CACHE_SIZE entries — evicts an entry when full.
    {
        let mut module_cache = state.module_cache.write().await;
        bounded_insert(
            &mut module_cache,
            path.clone(),
            transform_output.code.clone(),
            MAX_MODULE_CACHE_SIZE,
        );
    }

    // CSS files: serve as JS module with style injection for dev mode HMR
    if transform_output.is_css {
        let css_code = &transform_output.code;

        // CSS Modules: generate export object with scoped class names
        if let Some(ref css_module_map) = transform_output.css_modules {
            let mut exports = String::new();
            for (original, scoped) in css_module_map {
                // JSON-quote both sides: class names like `my-class` are not
                // valid bare identifiers.
                exports.push_str(&format!(
                    "  {}: {},\n",
                    serde_json::to_string(original).unwrap_or_else(|_| "\"\"".to_string()),
                    serde_json::to_string(scoped).unwrap_or_else(|_| "\"\"".to_string())
                ));
            }
            // Rewrite class names in CSS to scoped versions
            let mut scoped_css = css_code.clone();
            for (original, scoped) in css_module_map {
                let pattern = format!(".{}", original);
                let replacement = format!(".{}", scoped);
                scoped_css = scoped_css.replace(&pattern, &replacement);
            }
            let js_module = format!(
                r#"const __css = {};
const __styleId = '__pledge_style_' + {};
let __existing = document.getElementById(__styleId);
if (!__existing) {{
  __existing = document.createElement('style');
  __existing.id = __styleId;
  document.head.appendChild(__existing);
}}
__existing.textContent = __css;
export default {{}};
export {{}};
"#,
                serde_json::to_string(&scoped_css).unwrap_or_else(|_| "\"\"".to_string()),
                serde_json::to_string(&path).unwrap_or_else(|_| "\"\"".to_string())
            );
            // Replace the empty export with actual CSS module exports
            let js_module = js_module.replace(
                "export default {};\nexport {};\n",
                &format!("export default {{\n{}}};\n", exports),
            );
            if js_module.len() > MAX_RESPONSE_SIZE {
                return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
            }
            return (
                [
                    (
                        header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    ),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js_module,
            )
                .into_response();
        }

        // Regular CSS: inject as JS module that creates a <style> tag
        let js_module = format!(
            r#"const __css = {};
const __styleId = '__pledge_style_' + {};
let __existing = document.getElementById(__styleId);
if (!__existing) {{
  __existing = document.createElement('style');
  __existing.id = __styleId;
  document.head.appendChild(__existing);
}}
__existing.textContent = __css;
// HMR: update style tag on hot reload
if (import.meta.hot) {{
  import.meta.hot.accept();
}}
"#,
            serde_json::to_string(css_code).unwrap_or_else(|_| "\"\"".to_string()),
            serde_json::to_string(&path).unwrap_or_else(|_| "\"\"".to_string())
        );

        if js_module.len() > MAX_RESPONSE_SIZE {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
        }

        return (
            [
                (
                    header::CONTENT_TYPE,
                    "application/javascript; charset=utf-8",
                ),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            js_module,
        )
            .into_response();
    }

    // Handle extracted CSS from SFCs (Vue/Svelte/Astro) — inject as style tag
    if let Some(ref extracted_css) = transform_output.extracted_css {
        let style_id = format!(
            "__pledge_style_{}",
            path.replace(|c: char| !c.is_alphanumeric(), "_")
        );
        let css_inject = format!(
            r#"
const __css = {};
const __styleId = '{}';
let __existing = document.getElementById(__styleId);
if (!__existing) {{
  __existing = document.createElement('style');
  __existing.id = __styleId;
  document.head.appendChild(__existing);
}}
__existing.textContent = __css;
"#,
            serde_json::to_string(extracted_css).unwrap_or_else(|_| "\"\"".to_string()),
            style_id
        );
        // Prepend CSS injection to the module code
        let transformed = rewrite_imports(
            &format!("{}\n{}", css_inject, transform_output.code),
            &path,
            &state.config.resolve_alias,
            &state.prebundled_deps,
            &state.config.root,
        );
        // Continue with normal JS module handling below using the combined code
        let transform_output = pledgepack_core::transform::TransformOutput {
            code: transformed,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        };
        // Fall through to JS handling by re-running the logic below
        return serve_js_module(&path, &transform_output.code, None, &state).await;
    }

    // JS/TS files: rewrite imports and add HMR boundary
    let transformed = rewrite_imports(
        &transform_output.code,
        &path,
        &state.config.resolve_alias,
        &state.prebundled_deps,
        &state.config.root,
    );
    // Chain the plugins' map into the built-in transform's map so the browser
    // maps back to the file on disk, not the plugin's intermediate output.
    let source_map = match (
        transform_output.source_map.as_deref(),
        plugin_map.as_deref(),
    ) {
        (Some(outer), Some(inner)) => Some(
            pledgepack_core::sourcemap_compose::compose_source_maps(outer, inner)
                .unwrap_or_else(|| outer.to_string()),
        ),
        (Some(outer), None) => Some(outer.to_string()),
        _ => None,
    };
    serve_js_module(&path, &transformed, source_map.as_deref(), &state).await
}

/// Run the plugin `load` hook (replacing `code` when a plugin provides the
/// module) and then the chained `transform` hooks for `id`. Returns the final
/// code and a source map tracing it back to the module's origin (`None` when
/// no plugin in the chain supplied a usable map).
async fn run_plugin_module_hooks(
    hooks: &PluginHookService,
    id: &str,
    code: String,
) -> Result<(String, Option<String>)> {
    match hooks.load(id).await? {
        Some(loaded) => run_plugin_transform(hooks, id, loaded.code, loaded.map).await,
        None => run_plugin_transform(hooks, id, code, None).await,
    }
}

/// The chained `transform` half of [`run_plugin_module_hooks`]: `map` is the
/// map already relating `code` to the module's origin (from `load`).
async fn run_plugin_transform(
    hooks: &PluginHookService,
    id: &str,
    code: String,
    map: Option<String>,
) -> Result<(String, Option<String>)> {
    let Some(t) = hooks.transform(&code, id).await? else {
        return Ok((code, map));
    };
    let map = match (t.map, map.as_deref()) {
        // Both supplied maps: compose (transform output -> load output -> origin).
        (Some(t_map), Some(l_map)) => {
            pledgepack_core::sourcemap_compose::compose_source_maps(&t_map, l_map)
        }
        // The input has no map (it IS the origin): the transform map is relative to it.
        (Some(t_map), None) => Some(t_map),
        // Code changed without a map: lineage broken, report none.
        (None, _) => None,
    };
    Ok((t.code, map))
}

/// Respond to a failing plugin hook the same way a failing built-in
/// transform does: push an error to connected HMR clients (overlay) and serve
/// a module that throws with the message.
fn plugin_hook_error_response(
    state: &Arc<DevServerState>,
    path: &str,
    file_path: &str,
    e: &anyhow::Error,
) -> Response {
    let message = format!("{e:#}");
    let location = pledge_diagnostics::parse_location(&message);
    let _ = state.hmr_tx.send(HmrUpdate {
        update_type: "error".to_string(),
        path: path.to_string(),
        message: Some(message.clone()),
        file: Some(file_path.to_string()),
        css: None,
        stack: Some(message.clone()),
        line: location.map(|l| l.0 as u32),
        column: location.map(|l| l.1 as u32),
        deps: Vec::new(),
        full_reload: None,
        diff: None,
        full_code: None,
        module_map: None,
    });
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        transform_error_module(path, &message),
    )
        .into_response()
}

/// Serve a JS module with HMR polyfill, dependency tracking, and source maps
async fn serve_js_module(
    path: &str,
    transformed: &str,
    source_map: Option<&str>,
    state: &Arc<DevServerState>,
) -> Response {
    // Track imports in the dependency graph for cascading HMR updates
    // Bounded to MAX_IMPORT_GRAPH_SIZE entries — evicts an entry when full.
    {
        let imports = extract_imports(transformed);
        if !imports.is_empty() {
            let mut graph = state.import_graph.write().await;
            for dep in &imports {
                let normalized = normalize_module_path(dep, path);
                if !graph.contains_key(&normalized) && graph.len() >= MAX_IMPORT_GRAPH_SIZE {
                    // Evict an entry to make room (approximate eviction —
                    // HashMap keeps no insertion order)
                    if let Some(evict_key) = graph.keys().next().cloned() {
                        graph.remove(&evict_key);
                    }
                }
                let dependents = graph.entry(normalized).or_default();
                if !dependents.iter().any(|d| d == path) {
                    dependents.push(path.to_string());
                }
            }
        }
    }

    // Inject import.meta.hot polyfill with accept(), dispose(), invalidate(), and data
    let hmr_polyfill = format!(
        r#"
// Pledge HMR polyfill — import.meta.hot API
if (!import.meta.hot) {{
  const __pledge_hot_id = '{}';
  const __pledge_hot_data = {{}};
  const __pledge_hot_dispose_callbacks = [];
  const __pledge_hot_accept_callbacks = [];
  // Register callbacks globally so the HMR update handler can find them by module path
  window.__pledge_hot_accept_callbacks = window.__pledge_hot_accept_callbacks || {{}};
  window.__pledge_hot_accept_callbacks[__pledge_hot_id] = __pledge_hot_accept_callbacks;
  window.__pledge_hot_dispose_callbacks = window.__pledge_hot_dispose_callbacks || {{}};
  window.__pledge_hot_dispose_callbacks[__pledge_hot_id] = __pledge_hot_dispose_callbacks;
  import.meta.hot = {{
    data: __pledge_hot_data,
    accept(cb) {{
      if (typeof cb === 'function') __pledge_hot_accept_callbacks.push(cb);
    }},
    dispose(cb) {{
      if (typeof cb === 'function') __pledge_hot_dispose_callbacks.push(cb);
    }},
    invalidate() {{
      console.log('[pledge] HMR invalidate:', __pledge_hot_id);
      window.__pledge_hmr_invalidate = true;
      location.reload();
    }},
    __run_dispose() {{
      __pledge_hot_dispose_callbacks.forEach(cb => {{
        try {{ cb(__pledge_hot_data); }} catch(e) {{ console.error('[pledge] HMR dispose error:', e); }}
      }});
      __pledge_hot_dispose_callbacks.length = 0;
    }},
    __run_accept(newModule) {{
      __pledge_hot_accept_callbacks.forEach(cb => {{
        try {{ cb(newModule); }} catch(e) {{ console.error('[pledge] HMR accept error:', e); }}
      }});
    }}
  }};
}}
"#,
        path
    );

    // Add HMR boundary code for JS/TS files
    let mut module_with_hmr = if path.ends_with(".tsx")
        || path.ends_with(".jsx")
        || path.ends_with(".ts")
        || path.ends_with(".js")
    {
        format!(
            "{}\n{}\nif (import.meta.hot) {{\nimport.meta.hot.accept();\n}}",
            hmr_polyfill, transformed
        )
    } else {
        format!("{}\n{}", hmr_polyfill, transformed)
    };

    // Append sourceMappingURL comment for debuggable stack traces
    if let Some(sm) = source_map
        && !sm.is_empty()
    {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(sm.as_bytes());
        module_with_hmr.push_str(&format!(
            "\n//# sourceMappingURL=data:application/json;base64,{}\n",
            encoded
        ));
    }

    if module_with_hmr.len() > MAX_RESPONSE_SIZE {
        return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
    }

    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        module_with_hmr,
    )
        .into_response()
}

/// Normalize a relative import specifier to an absolute module path
fn normalize_module_path(specifier: &str, importer: &str) -> String {
    if specifier.starts_with('/') {
        return specifier.trim_start_matches('/').to_string();
    }
    if specifier.starts_with("./") || specifier.starts_with("../") {
        // Resolve relative to importer's directory
        let importer_dir = importer.rfind('/').map(|i| &importer[..i]).unwrap_or("");
        let parts: Vec<&str> = specifier.split('/').collect();
        let mut path_parts: Vec<&str> = importer_dir.split('/').filter(|s| !s.is_empty()).collect();
        for part in parts {
            match part {
                "." => {}
                ".." => {
                    path_parts.pop();
                }
                _ => {
                    path_parts.push(part);
                }
            }
        }
        return path_parts.join("/");
    }
    specifier.to_string()
}

/// Rewrite require() calls in CJS node_modules files to use the __pledge_require shim.
/// The CJS→ESM wrapper provides a `require` function that returns the cached module.
/// For relative requires like require("./cjs/react.development.js"), we need to resolve
/// them to absolute paths so the browser can fetch them as ESM modules.
fn rewrite_cjs_requires(source: &str, module_path: &str) -> String {
    // Get the directory of the current module for resolving relative requires
    let module_dir = module_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");

    // Replace require("./...") and require("./...") with resolved absolute paths
    // The CJS wrapper already provides `require`, so we just need to make sure
    // relative paths resolve correctly. We convert relative require paths to
    // absolute paths from the server root.
    let mut result = source.to_string();

    // Pattern: require("./something") or require('../something')
    // Replace with require("/node_modules/<pkg>/something")
    while let Some(start) = result.find("require(\".") {
        if let Some(end) = result[start..].find("\")") {
            let quote_start = start + 8; // after require("
            let quote_end = start + end; // position of closing "
            let relative_path = &result[quote_start..quote_end];

            // Resolve relative to module directory
            let resolved = resolve_relative_require(module_dir, relative_path);

            result = format!(
                "{}require(\"{}\"){}",
                &result[..start],
                resolved,
                &result[quote_end + 2..]
            );
        } else {
            break;
        }
    }

    // Also handle require('./...') with single quotes
    while let Some(start) = result.find("require('.") {
        if let Some(end) = result[start..].find("')") {
            let quote_start = start + 8;
            let quote_end = start + end;
            let relative_path = &result[quote_start..quote_end];

            let resolved = resolve_relative_require(module_dir, relative_path);

            result = format!(
                "{}require('{}'){}",
                &result[..start],
                resolved,
                &result[quote_end + 2..]
            );
        } else {
            break;
        }
    }

    result
}

/// Resolve a relative require() path to an absolute /node_modules/... path.
/// Handles ./, ../, and multi-level ../../ paths correctly.
fn resolve_relative_require(module_dir: &str, relative_path: &str) -> String {
    if let Some(rest) = relative_path.strip_prefix("./") {
        format!("/node_modules/{}/{}", module_dir, rest)
    } else if let Some(rest) = relative_path.strip_prefix("../") {
        // Count and strip all ../ segments
        let mut dir = module_dir.to_string();
        let mut rest = rest;
        while rest.starts_with("../") {
            dir = dir
                .rsplit_once('/')
                .map(|(d, _)| d)
                .unwrap_or(&dir)
                .to_string();
            rest = &rest[3..];
        }
        // Handle remaining ../ (if rest is just "../foo" without another "../")
        // Actually rest no longer starts with ../, so it's the final path
        format!("/node_modules/{}/{}", dir, rest)
    } else {
        relative_path.to_string()
    }
}

/// Rewrite import/export specifiers to browser-compatible URLs.
/// Converts `./foo` → `./foo.js`, `../bar` → `../bar.js`, etc.
/// Also rewrites resolve aliases (e.g., `@/components` → `/src/components`).
/// Bare specifiers like `react` are left as-is (browser must resolve via import map).
fn rewrite_imports(
    code: &str,
    current_module_path: &str,
    aliases: &[pledgepack_core::PathAlias],
    prebundled_deps: &std::collections::HashMap<String, String>,
    root: &std::path::Path,
) -> String {
    // Directory of the importing module on disk, used to pick the real
    // extension for extensionless specifiers (`./util` → `./util.ts` when
    // `util.ts` exists, not a blanket `.tsx`).
    let importer_dir = root
        .join(current_module_path.trim_start_matches('/'))
        .parent()
        .map(|p| p.to_path_buf());
    let mut result = code.to_string();

    // Rewrite relative import/export specifiers to include .tsx extension
    // This is a simple regex-like approach; Oxc could do this in AST but
    // for dev server speed, string rewriting is sufficient.
    for pattern in [
        "from \"",
        "from '",
        "import \"",
        "import '",
        "import(",
        "export * from \"",
        "export * from '",
    ] {
        let mut search_from = 0;
        while let Some(pos) = result[search_from..].find(pattern) {
            let pos = search_from + pos;
            let after_pattern = pos + pattern.len();
            let rest = &result[after_pattern..];

            // Find the closing quote
            let closing_quote = if pattern.ends_with('"') {
                '"'
            } else if pattern.ends_with('\'') {
                '\''
            } else {
                '('
            }; // for "import("

            if closing_quote == '(' {
                // Dynamic import: find the string inside
                // Only a string literal directly inside the parens counts:
                // `import(name)` must not pick up an unrelated later string.
                let trimmed = rest.trim_start();
                if let Some(quote_char) = trimmed.chars().next().filter(|c| matches!(c, '"' | '\''))
                {
                    let spec_start = (rest.len() - trimmed.len()) + 1;
                    let spec_rest = &rest[spec_start..];
                    if let Some(end) = spec_rest.find(quote_char) {
                        let specifier = &spec_rest[..end];
                        let abs_start = after_pattern + spec_start;
                        let abs_end = abs_start + end;
                        if specifier.starts_with("./") || specifier.starts_with("../") {
                            let new_spec = add_js_extension(specifier, importer_dir.as_deref());
                            result.replace_range(abs_start..abs_end, &new_spec);
                        } else if let Some(url) = prebundled_deps.get(specifier) {
                            // Bare dynamic import of a pre-bundled dep.
                            let url = url.clone();
                            result.replace_range(abs_start..abs_end, &url);
                        }
                    }
                }
                // Resume right after the `(`: `+ 1` could land inside a
                // multi-byte character and panic on the next slice.
                search_from = after_pattern;
                continue;
            }

            if let Some(end) = rest.find(closing_quote) {
                let specifier = &rest[..end];
                if specifier.starts_with("./") || specifier.starts_with("../") {
                    let new_spec = add_js_extension(specifier, importer_dir.as_deref());
                    let abs_start = after_pattern;
                    let abs_end = abs_start + end;
                    result.replace_range(abs_start..abs_end, &new_spec);
                    // Advance past the rewritten specifier
                    search_from = abs_end + 1;
                } else if let Some(url) = prebundled_deps.get(specifier) {
                    // Bare import of a pre-bundled dep → its .pledge-deps URL.
                    let url = url.clone();
                    let abs_end = after_pattern + end;
                    result.replace_range(after_pattern..abs_end, &url);
                    search_from = after_pattern + url.len() + 1;
                } else {
                    // Not a relative path, advance past it
                    search_from = after_pattern + end + 1;
                }
            } else {
                break;
            }
        }
    }

    // Rewrite resolve aliases (e.g., "@/components" → "/src/components")
    for alias in aliases {
        let from_with_slash = format!("{}/", alias.from);
        let from_exact = alias.from.as_str();
        for pattern in [
            "from \"",
            "from '",
            "import \"",
            "import '",
            "import(",
            "export * from \"",
            "export * from '",
        ] {
            // Match alias as exact or prefix
            let alias_prefixes = [from_exact, &from_with_slash];
            for &alias_prefix in &alias_prefixes {
                let search = format!("{}{}", pattern, alias_prefix);
                let mut search_from = 0;
                while let Some(pos) = result[search_from..].find(&search) {
                    let pos = search_from + pos;
                    // Replace the alias with the target path
                    let replacement = format!("{}{}", pattern, alias.to);
                    result
                        .replace_range(pos..pos + pattern.len() + alias_prefix.len(), &replacement);
                    // Advance past the replacement to avoid infinite loops
                    search_from = pos + replacement.len();
                }
            }
        }
    }

    result
}

/// Add a JS extension to a relative specifier if it doesn't have one.
/// When `importer_dir` is known, probe the filesystem for the real extension
/// (`./util` next to `util.ts` → `./util.ts`, directory → `./dir/index.ts`);
/// only fall back to `.tsx` when nothing on disk matches (virtual/plugin ids).
fn add_js_extension(specifier: &str, importer_dir: Option<&std::path::Path>) -> String {
    // Check if it already has a JS-compatible extension
    let has_ext = specifier.ends_with(".js")
        || specifier.ends_with(".jsx")
        || specifier.ends_with(".ts")
        || specifier.ends_with(".tsx")
        || specifier.ends_with(".mjs")
        || specifier.ends_with(".json")
        || specifier.ends_with(".css");

    if specifier.ends_with(".json") {
        // A JSON specifier in import position is a module import: mark it so
        // the server wraps it as a module (plain fetches get raw JSON).
        format!("{specifier}?import")
    } else if has_ext {
        specifier.to_string()
    } else if let Some(dir) = importer_dir {
        let target = dir.join(specifier);
        for ext in ["ts", "tsx", "js", "jsx", "mjs"] {
            if target.with_extension(ext).is_file() {
                return format!("{specifier}.{ext}");
            }
        }
        // Directory import: `./components` → `./components/index.<ext>`
        if target.is_dir() {
            for ext in ["ts", "tsx", "js", "jsx", "mjs"] {
                if target.join(format!("index.{ext}")).is_file() {
                    return format!("{specifier}/index.{ext}");
                }
            }
        }
        format!("{specifier}.tsx")
    } else {
        // Use .tsx as default — the dev server module_handler tries
        // alternative extensions (.tsx, .ts, .jsx, .js) when serving
        format!("{}.tsx", specifier)
    }
}

/// WebSocket endpoint for HMR updates
/// Supports per-message deflate compression (feature 12) via axum's WebSocket upgrade
async fn hmr_websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<DevServerState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_hmr_connection(socket, state))
}

async fn handle_hmr_connection(socket: axum::extract::ws::WebSocket, state: Arc<DevServerState>) {
    info!("HMR client connected");

    // Split socket into sender and receiver
    let (mut socket_tx, mut socket_rx) = socket.split();

    // Send initial connection confirmation
    let hello = serde_json::json!({
        "type": "connected",
        "message": "Pledge HMR connected"
    });
    // Must be awaited: a `SinkExt::send` future that is merely dropped never
    // writes anything, so clients previously never received this message.
    if socket_tx
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    // Register this client to receive HMR updates
    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<HmrUpdate>();
    {
        let mut clients = state.hmr_clients.write().await;
        clients.push(client_tx);
        // Goal 76: a "currently connected clients" diagnostic — logged on
        // every connect/disconnect rather than only queryable on demand, so
        // it shows up in the same terminal output a developer is already
        // watching, without needing a separate endpoint to poll.
        info!("HMR client connected ({} total)", clients.len());
    }

    // Spawn a task to forward HMR updates to this WebSocket client
    // Uses binary messages for larger payloads (compression benefit)
    let send_task = tokio::spawn(async move {
        while let Some(update) = client_rx.recv().await {
            let json = match serde_json::to_string(&update) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to serialize HMR update: {}", e);
                    continue; // Skip this update instead of sending garbage
                }
            };
            // For small messages, use text; for larger ones, use binary
            // (per-message-deflate is not enabled — see the known-limitation note above)
            if json.len() < 4096 {
                if socket_tx.send(Message::Text(json.into())).await.is_err() {
                    break;
                }
            } else {
                if socket_tx
                    .send(Message::Binary(json.into_bytes().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });

    // Keep connection alive — handle incoming messages (ping/pong)
    while let Some(msg) = socket_rx.next().await {
        match msg {
            Ok(Message::Ping(_)) => {
                // axum auto-responds to pings, but we can also handle explicitly
            }
            Ok(Message::Close(_)) => {
                info!("HMR client disconnected");
                break;
            }
            _ => {}
        }
    }

    // Clean up: remove this client from the registered list
    send_task.abort();
    // Wait for the aborted task to drop its receiver so the `is_closed()`
    // sweep below removes this client's sender now, not at some later
    // disconnect.
    let _ = send_task.await;
    {
        let mut clients = state.hmr_clients.write().await;
        clients.retain(|tx| !tx.is_closed());
        info!("HMR client disconnected ({} remaining)", clients.len());
    }
}

/// Native file watcher — uses platform-specific APIs (ReadDirectoryChangesW/inotify/FSEvents)
/// with automatic fallback to notify crate
/// Load `<root>/plugins/*.js` into a JS plugin host under the configured
/// signing/capability policy (deny-unsigned by default; opt out with
/// `plugin_security.require_signed = false`) and run `configureServer`.
/// Returns `None` when there is no plugins dir or the trust check failed.
fn load_dev_plugin_host(config: &PledgeConfig) -> Option<JsPluginHost> {
    load_dev_plugin_host_with(config, true)
}

/// [`load_dev_plugin_host`], optionally skipping `configureServer` (the
/// per-module hook host is a second instance of the same plugins and must not
/// run their server-configuration side effects twice).
fn load_dev_plugin_host_with(
    config: &PledgeConfig,
    run_configure_server: bool,
) -> Option<JsPluginHost> {
    let plugins_dir = config.root.join("plugins");
    if !plugins_dir.is_dir() {
        return None;
    }
    let mut plugin_host = JsPluginHost::new();
    let (verifier, auditor) = config.plugin_security.policy();
    if let Some(v) = verifier {
        plugin_host = plugin_host.with_signing_verifier(v);
    }
    if let Some(a) = auditor {
        plugin_host = plugin_host.with_capability_auditor(a);
    }
    match plugin_host.load_dir(&plugins_dir) {
        Ok(()) => {
            let middlewares = if run_configure_server {
                plugin_host.configure_server()
            } else {
                Vec::new()
            };
            for mw in &middlewares {
                info!(
                    "[plugin:{}] configureServer registered middleware ({} bytes)",
                    mw.plugin_name,
                    mw.source.len()
                );
            }
            Some(plugin_host)
        }
        Err(e) => {
            warn!(
                "Skipping plugins/ dir plugins — trust check failed: {e} \
                 (set plugin_security.require_signed = false to load unsigned plugins)"
            );
            None
        }
    }
}

/// What the dev server should do with a changed file after the plugins'
/// `handleHotUpdate` hooks have run (Vite semantics: the returned module
/// list *replaces* the default affected-module set).
#[derive(Debug, PartialEq, Eq)]
enum HotUpdatePlan {
    /// No plugin claimed the change — normal single-file update.
    Default,
    /// A plugin returned an empty list — suppress the update entirely.
    Suppress,
    /// A plugin returned an explicit module list — update exactly those.
    Modules(Vec<String>),
}

fn plan_hot_update(result: Option<pledgepack_js_plugin_host::HotUpdateResult>) -> HotUpdatePlan {
    match result {
        None => HotUpdatePlan::Default,
        Some(r) if r.module_ids.is_empty() => HotUpdatePlan::Suppress,
        Some(r) => HotUpdatePlan::Modules(r.module_ids),
    }
}

fn start_native_file_watcher(
    root: PathBuf,
    tx: mpsc::UnboundedSender<HmrUpdate>,
    server_entry: Option<String>,
    dev_config: &PledgeConfig,
) {
    let mut plugin_host = load_dev_plugin_host(dev_config);
    let config = watcher::WatcherConfig::default();
    let rx = watcher::start_watcher(&root, config);

    // Determine server-only directories/patterns from server_entry
    let server_dirs = compute_server_dirs(&root, &server_entry);

    // Process events from the native watcher and send HMR updates
    while let Ok(event) = rx.recv() {
        let rel_path = normalize_path(event.path.strip_prefix(&root).unwrap_or(&event.path));

        let ext = event
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        // Check if this is a server-only file change
        let is_server_file = is_server_file(&rel_path, &server_dirs, &server_entry);

        if is_server_file {
            info!(
                "Server file changed: {} — triggering graceful reload",
                rel_path
            );

            // Send "server-reload" notification so clients know to expect a brief pause
            let reload_start = HmrUpdate {
                update_type: "server-reload".to_string(),
                path: rel_path.clone(),
                message: Some("Server code changed — reloading...".to_string()),
                file: None,
                css: None,
                stack: None,
                line: None,
                column: None,
                deps: Vec::new(),
                full_reload: None,
                diff: None,
                full_code: None,
                module_map: None,
            };
            let _ = tx.send(reload_start);

            // Brief delay to let clients process the notification
            std::thread::sleep(std::time::Duration::from_millis(100));

            // Send "server-reload-complete" so clients know the server is back
            let reload_done = HmrUpdate {
                update_type: "server-reload-complete".to_string(),
                path: rel_path.clone(),
                message: Some("Server code reloaded successfully".to_string()),
                file: None,
                css: None,
                stack: None,
                line: None,
                column: None,
                deps: Vec::new(),
                full_reload: None,
                diff: None,
                full_code: None,
                module_map: None,
            };
            let _ = tx.send(reload_done);
            continue;
        }

        // Let plugins' `handleHotUpdate` hooks decide which modules to update.
        let plan = match plugin_host.as_mut() {
            Some(host) => {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                plan_hot_update(host.handle_hot_update(&rel_path, timestamp))
            }
            None => HotUpdatePlan::Default,
        };
        let targets: Vec<String> = match plan {
            HotUpdatePlan::Default => vec![rel_path.clone()],
            HotUpdatePlan::Suppress => {
                info!("handleHotUpdate suppressed HMR update for {}", rel_path);
                continue;
            }
            HotUpdatePlan::Modules(ids) => ids,
        };

        for target in targets {
            // Read the new file content for diff computation
            let target_path = if target == rel_path {
                event.path.clone()
            } else {
                root.join(&target)
            };
            let target_ext = target_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(ext);
            let new_content = std::fs::read_to_string(&target_path).ok();

            let update = HmrUpdate {
                update_type: "update".to_string(),
                path: target,
                message: None,
                file: None,
                css: if target_ext == "css" {
                    new_content.clone()
                } else {
                    None
                },
                stack: None,
                line: None,
                column: None,
                deps: Vec::new(),
                full_reload: None,
                diff: None,
                full_code: new_content,
                module_map: None,
            };
            let _ = tx.send(update);
        }
    }
}

/// Compute server-only directories from the server_entry config
fn compute_server_dirs(root: &std::path::Path, server_entry: &Option<String>) -> Vec<String> {
    let mut dirs = Vec::new();
    if let Some(entry) = server_entry {
        // Derive server directory from entry path (e.g., "server/index.ts" → "server")
        if let Some(parent) = std::path::Path::new(entry).parent()
            && !parent.as_os_str().is_empty()
        {
            dirs.push(normalize_path(parent));
        }
    }
    // Common SSR/API directories
    for common in &["api", "server", "src/api", "src/server", "app/api"] {
        if root.join(common).is_dir() {
            dirs.push(common.to_string());
        }
    }
    dirs
}

/// Check if a file path is a server-only file
fn is_server_file(rel_path: &str, server_dirs: &[String], server_entry: &Option<String>) -> bool {
    // Check if it's the server entry file itself
    if let Some(entry) = server_entry
        && rel_path == entry.as_str()
    {
        return true;
    }
    // Check if it's in a server directory
    for dir in server_dirs {
        if rel_path.starts_with(dir) {
            return true;
        }
    }
    false
}

/// Detect multi-entry HTML files in the project root
/// Looks for index.html, admin.html, mobile.html, etc.
fn detect_entries(config: &PledgeConfig) -> Vec<EntryConfig> {
    let mut entries = Vec::new();

    // Check for explicit html_entry in config
    if let Some(ref html_entry) = config.html_entry {
        let entry_module = config.entry.first().cloned().unwrap_or_default();
        entries.push(EntryConfig {
            name: "index".to_string(),
            html_file: html_entry.clone(),
            entry_module,
        });
        return entries;
    }

    // Auto-detect HTML files in root and src directories
    let check_dirs = [config.root.clone(), config.root.join("src")];

    for dir in &check_dirs {
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries_iter) = std::fs::read_dir(dir) {
            for entry in entries_iter.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".html") {
                    continue;
                }

                let entry_name = name.trim_end_matches(".html").to_string();

                // Skip non-entry HTML files (like error pages)
                if entry_name == "error" || entry_name == "404" {
                    continue;
                }

                // Find the corresponding entry module
                let entry_module = if entry_name == "index" {
                    config
                        .entry
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "src/index.tsx".to_string())
                } else {
                    // Look for matching JS/TS file
                    let candidates = [
                        format!("src/{}.tsx", entry_name),
                        format!("src/{}.ts", entry_name),
                        format!("src/{}/index.tsx", entry_name),
                        format!("src/{}/index.ts", entry_name),
                        format!("{}.tsx", entry_name),
                        format!("{}.ts", entry_name),
                    ];
                    candidates
                        .iter()
                        .find(|c| config.root.join(c).exists())
                        .cloned()
                        .unwrap_or_else(|| format!("src/{}.tsx", entry_name))
                };

                let html_rel = path
                    .strip_prefix(&config.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                entries.push(EntryConfig {
                    name: entry_name,
                    html_file: html_rel,
                    entry_module,
                });
            }
        }
    }

    // If no entries found, default to index
    if entries.is_empty() {
        entries.push(EntryConfig {
            name: "index".to_string(),
            html_file: "index.html".to_string(),
            entry_module: config
                .entry
                .first()
                .cloned()
                .unwrap_or_else(|| "src/index.tsx".to_string()),
        });
    }

    entries
}

/// Entry index handler for multi-entry dev server (feature 13)
/// Serves the HTML file for a specific entry point (e.g., /admin → admin.html)
async fn entry_index_handler(
    State(state): State<Arc<DevServerState>>,
    Path(entry_name): Path<String>,
) -> Response {
    let entries = state.entries.read().await;
    let entry = entries.iter().find(|e| e.name == entry_name);

    let entry = match entry {
        Some(e) => e,
        None => return (StatusCode::NOT_FOUND, "Entry not found").into_response(),
    };
    let html_path = state.config.root.join(&entry.html_file);

    let mut html = match std::fs::read_to_string(&html_path) {
        Ok(content) => content,
        Err(_) => {
            // Auto-generate HTML shell from layout.tsx
            let (html_attrs, head_content) =
                match shell_generator::try_extract_shell_from_project(&state.config.root) {
                    Some((attrs, head)) => (attrs, head),
                    None => (
                        "lang=\"en\"".to_string(),
                        format!("<title>Pledge — {}</title>", entry.name),
                    ),
                };
            let import_map = generate_import_map(&state.config, &state.prebundled_deps);
            shell_generator::generate_html_shell(&html_attrs, &head_content, "", &import_map)
        }
    };

    // Inject HMR client script (same as index_handler)
    // Inject import map (skip if already present from shell generator)
    if !html.contains("type=\"importmap\"") {
        let import_map = generate_import_map(&state.config, &state.prebundled_deps);
        if !import_map.is_empty() {
            let map_tag = format!("<script type=\"importmap\">\n{}\n</script>\n", import_map);
            if html.contains("</head>") {
                html = html.replace("</head>", &format!("{}\n</head>", map_tag));
            } else if html.contains("<body") {
                html = html.replace("<body", &format!("{}\n<body", map_tag));
            } else {
                html = format!("{}\n{}", map_tag, html);
            }
        }
    }

    inject_hmr_client(&mut html);

    Html(html).into_response()
}

/// Broadcast HMR updates to all connected WebSocket clients
/// Also computes dependent modules from the import graph for cascading HMR updates
/// and computes line-level diffs for partial HMR updates (feature 10)
/// and tracks import pattern changes for on-demand optimization (feature 15)
async fn hmr_broadcast_loop(
    state: Arc<DevServerState>,
    mut hmr_rx: mpsc::UnboundedReceiver<HmrUpdate>,
) {
    while let Some(mut update) = hmr_rx.recv().await {
        info!("HMR update: {} (type: {})", update.path, update.update_type);

        // Compute line-level diff for partial HMR updates (feature 10)
        if update.update_type == "update"
            && let Some(ref full_code) = update.full_code
        {
            let module_cache = state.module_cache.read().await;
            if let Some(old_code) = module_cache.get(&update.path) {
                let diff = hmr_diff::compute_diff(old_code, full_code);
                if diff.is_small_default() {
                    update.diff = Some(diff);
                } else {
                    update.diff = None;
                }
            }
            drop(module_cache);

            let mut module_cache = state.module_cache.write().await;
            bounded_insert(
                &mut module_cache,
                update.path.clone(),
                full_code.clone(),
                MAX_MODULE_CACHE_SIZE,
            );
        }

        // CSS Modules: compute class name mappings for HMR remapping
        // When a .module.css changes, transform it to get the new scoped class names
        // and include the mapping in the update so clients can re-import the module
        if update.update_type == "update"
            && update.path.ends_with(".module.css")
            && let Some(ref full_code) = update.full_code
        {
            let mut lazy_pipeline = state.lazy_pipeline.write().await;
            lazy_pipeline.ensure_initialized();
            let kind = ModuleKind::from_extension(".css");
            if let Ok(css_output) =
                pledge_transform::transform(full_code, kind, &update.path, false, &state.config)
                && let Some(ref css_module_map) = css_output.css_modules
            {
                let map: serde_json::Map<String, serde_json::Value> = css_module_map
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect();
                update.module_map = Some(serde_json::Value::Object(map));
            }
        }

        // Track import pattern changes for on-demand optimization (feature 15)
        if update.update_type == "update"
            && let Some(ref new_code) = update.full_code
        {
            let new_imports = extract_imports(new_code);
            let import_patterns = state.import_patterns.read().await;
            let old_imports = import_patterns.get(&update.path);
            let imports_changed = old_imports.is_none_or(|old| {
                old.len() != new_imports.len()
                    || old.iter().zip(new_imports.iter()).any(|(a, b)| a != b)
            });
            drop(import_patterns);

            if imports_changed {
                info!(
                    "Import patterns changed for {}, re-optimizing dependencies",
                    update.path
                );
                let mut import_patterns = state.import_patterns.write().await;
                import_patterns.insert(update.path.clone(), new_imports);
                let mut lazy_pipeline = state.lazy_pipeline.write().await;
                lazy_pipeline.mark_deps_dirty(&update.path);
            }
        }

        // Compute dependent modules that need cascading updates
        if update.update_type == "update" {
            let graph = state.import_graph.read().await;
            let mut deps = Vec::new();
            let mut visited = std::collections::HashSet::new();
            collect_dependents(&update.path, &graph, &mut deps, &mut visited);
            if !deps.is_empty() {
                update.deps = deps;
            }
        }

        // Broadcast to all registered client channels
        let clients = state.hmr_clients.read().await;
        for client_tx in clients.iter() {
            let _ = client_tx.send(update.clone());
        }
    }
}

/// Extract import specifiers from JS/TS source code for on-demand optimization tracking
fn extract_imports(code: &str) -> Vec<String> {
    let mut imports = Vec::new();
    for pattern in ["from \"", "from '", "import \"", "import '", "import("] {
        let mut search_from = 0;
        while let Some(pos) = code[search_from..].find(pattern) {
            let pos = search_from + pos;
            let after_pattern = pos + pattern.len();
            let rest = &code[after_pattern..];

            let closing_quote = if pattern.ends_with('"') {
                '"'
            } else if pattern.ends_with('\'') {
                '\''
            } else {
                '('
            };

            if closing_quote == '(' {
                // Only a string literal directly inside the parens counts:
                // `import(name)` must not pick up an unrelated later string.
                let trimmed = rest.trim_start();
                if let Some(quote_char) = trimmed.chars().next().filter(|c| matches!(c, '"' | '\''))
                {
                    let spec_start = (rest.len() - trimmed.len()) + 1;
                    let spec_rest = &rest[spec_start..];
                    if let Some(end) = spec_rest.find(quote_char) {
                        let specifier = &spec_rest[..end];
                        imports.push(specifier.to_string());
                    }
                }
                // Resume right after the `(`: `+ 1` could land inside a
                // multi-byte character and panic on the next slice.
                search_from = after_pattern;
                continue;
            }

            if let Some(end) = rest.find(closing_quote) {
                let specifier = &rest[..end];
                imports.push(specifier.to_string());
                search_from = after_pattern + end + 1;
            } else {
                break;
            }
        }
    }
    imports.sort();
    imports.dedup();
    imports
}

/// Recursively collect all modules that depend on the given path (directly or transitively)
fn collect_dependents(
    path: &str,
    graph: &std::collections::HashMap<String, Vec<String>>,
    deps: &mut Vec<String>,
    visited: &mut std::collections::HashSet<String>,
) {
    if visited.contains(path) {
        return;
    }
    visited.insert(path.to_string());

    // Try exact match and also try with common extensions
    let candidates = [
        path.to_string(),
        format!("{}.js", path),
        format!("{}.ts", path),
        format!("{}.tsx", path),
    ];
    for candidate in &candidates {
        if let Some(direct_deps) = graph.get(candidate) {
            for dep in direct_deps {
                if !deps.contains(dep) {
                    deps.push(dep.clone());
                }
                collect_dependents(dep, graph, deps, visited);
            }
        }
    }
}

/// Normalize a configured proxy prefix (`"/api"`, `"api/"`, `"/api/"`) to
/// `"/api"`. Returns `None` for an empty / root-only prefix, which would
/// shadow every app route.
fn normalize_proxy_prefix(path: &str) -> Option<String> {
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(format!("/{trimmed}"))
    }
}

/// Build the upstream URL for a proxied request.
///
/// `request_path` is the full incoming path (including the prefix) and
/// `query` the raw query string, if any — the query string must be forwarded
/// or `/api/users?id=1` silently loses its parameters.
fn proxy_upstream_url(
    target: &str,
    prefix: &str,
    rewrite: bool,
    request_path: &str,
    query: Option<&str>,
) -> String {
    let target = target.trim_end_matches('/');
    let rest = request_path.strip_prefix(prefix).unwrap_or(request_path);
    let mut url = if rewrite {
        format!("{target}{rest}")
    } else {
        format!("{target}{prefix}{rest}")
    };
    if let Some(q) = query
        && !q.is_empty()
    {
        url.push('?');
        url.push_str(q);
    }
    url
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

/// Register the configured `proxy` entries on `app`. Each entry serves both
/// `<prefix>` and `<prefix>/{*rest}` (HTTP and, when `ws` is set, WebSocket
/// upgrades on the same route — axum forbids two handlers on one path+method).
fn add_proxy_routes(mut app: Router, proxies: &[pledgepack_core::config::ProxyConfig]) -> Router {
    for proxy in proxies {
        let Some(prefix) = normalize_proxy_prefix(&proxy.path) else {
            warn!(
                "Ignoring proxy entry with empty path (target {}): it would shadow every route",
                proxy.target
            );
            continue;
        };
        info!(
            "Proxy: {} → {}{}{}",
            prefix,
            proxy.target,
            if proxy.rewrite { " (rewrite)" } else { "" },
            if proxy.ws { " (ws)" } else { "" }
        );
        let ctx = Arc::new((prefix.clone(), proxy.clone()));
        // `Result<..>` rather than `Option<..>`: `WebSocketUpgrade` has no
        // `OptionalFromRequestParts` impl, and a plain HTTP request must fall
        // through to the HTTP proxy instead of being rejected.
        let handler = move |ws: Result<
            WebSocketUpgrade,
            axum::extract::ws::rejection::WebSocketUpgradeRejection,
        >,
                            req: axum::extract::Request| {
            let ctx = ctx.clone();
            async move { proxy_request(ws.ok(), req, &ctx.0, &ctx.1).await }
        };
        app = app
            .route(&prefix, axum::routing::any(handler.clone()))
            .route(&format!("{prefix}/{{*rest}}"), axum::routing::any(handler));
    }
    app
}

async fn proxy_request(
    ws: Option<WebSocketUpgrade>,
    req: axum::extract::Request,
    prefix: &str,
    proxy: &pledgepack_core::config::ProxyConfig,
) -> Response {
    let target_url = proxy_upstream_url(
        &proxy.target,
        prefix,
        proxy.rewrite,
        req.uri().path(),
        req.uri().query(),
    );

    if let Some(ws) = ws
        && proxy.ws
    {
        return ws.on_upgrade(move |socket| ws_proxy_handler(socket, target_url));
    }

    proxy_handler(req, &target_url, &proxy.headers).await
}

/// Proxy handler for dev server API proxying.
/// Forwards the request (method, headers, body) to `target_url` and relays
/// the response. Redirects are NOT followed — they are relayed to the browser.
async fn proxy_handler(
    req: axum::extract::Request,
    target_url: &str,
    extra_headers: &std::collections::HashMap<String, String>,
) -> Response {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let Some(client) = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .ok()
        })
        .as_ref()
    else {
        return (StatusCode::BAD_GATEWAY, "Proxy client unavailable").into_response();
    };

    let (parts, body) = req.into_parts();
    info!(
        "Proxy: {} {} → {}",
        parts.method,
        parts.uri.path(),
        target_url
    );

    let req_method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);

    const MAX_PROXY_BODY: usize = 100 * 1024 * 1024; // 100 MB
    let body_bytes = match axum::body::to_bytes(body, MAX_PROXY_BODY).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Body too large").into_response();
        }
    };

    let mut request = client.request(req_method, target_url);
    for (name, value) in parts.headers.iter() {
        let n = name.as_str();
        // `host` must describe the upstream, `content-length` is recomputed,
        // and the dev-server access token must not leak to the backend.
        if is_hop_by_hop(n) || matches!(n, "host" | "content-length" | "x-pledge-token") {
            continue;
        }
        request = request.header(n, value.as_bytes());
    }
    for (k, v) in extra_headers {
        request = request.header(k.as_str(), v.as_str());
    }
    let request = request.body(body_bytes);

    match request.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("Proxy body read error: {}", e);
                    return (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e))
                        .into_response();
                }
            };

            let mut response = axum::response::Response::new(axum::body::Body::from(body));
            *response.status_mut() =
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            for (key, value) in headers.iter() {
                let k = key.as_str();
                if is_hop_by_hop(k) || k == "content-length" {
                    continue;
                }
                if let (Ok(name), Ok(val)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_bytes(value.as_bytes()),
                ) {
                    // `append`, not `insert`: repeated headers (Set-Cookie)
                    // must all survive.
                    response.headers_mut().append(name, val);
                }
            }
            response
        }
        Err(e) => {
            tracing::warn!("Proxy error: {}", e);
            (StatusCode::BAD_GATEWAY, format!("Proxy error: {}", e)).into_response()
        }
    }
}

/// WebSocket proxy handler — bridges client WebSocket to target WebSocket
async fn ws_proxy_handler(client_socket: axum::extract::ws::WebSocket, target_url: String) {
    // Convert http(s):// to ws(s)://
    let ws_url = target_url
        .replacen("http://", "ws://", 1)
        .replacen("https://", "wss://", 1);

    info!("WS Proxy: connecting to {}", ws_url);

    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    // Connect to the target WebSocket
    let (target_socket, _) = match tokio_tungstenite::connect_async(&ws_url).await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!("WS Proxy connection failed: {}", e);
            return;
        }
    };

    let (client_sink, client_stream) = client_socket.split();
    let (target_sink, target_stream) = target_socket.split();

    // Convert client messages to tungstenite messages and forward
    let client_to_target = client_stream
        .filter_map(|msg| async {
            match msg {
                Ok(axum::extract::ws::Message::Text(text)) => {
                    Some(Ok(Message::Text(text.to_string().into())))
                }
                Ok(axum::extract::ws::Message::Binary(bin)) => Some(Ok(Message::Binary(bin))),
                Ok(axum::extract::ws::Message::Ping(data)) => Some(Ok(Message::Ping(data))),
                Ok(axum::extract::ws::Message::Pong(data)) => Some(Ok(Message::Pong(data))),
                Ok(axum::extract::ws::Message::Close(_)) => Some(Ok(Message::Close(None))),
                Err(_) => None,
            }
        })
        .forward(target_sink);

    // Convert target messages to client messages and forward
    let target_to_client = target_stream
        .filter_map(|msg| async {
            match msg {
                Ok(Message::Text(text)) => Some(Ok(axum::extract::ws::Message::Text(
                    text.to_string().into(),
                ))),
                Ok(Message::Binary(bin)) => Some(Ok(axum::extract::ws::Message::Binary(bin))),
                Ok(Message::Ping(data)) => Some(Ok(axum::extract::ws::Message::Ping(data))),
                Ok(Message::Pong(data)) => Some(Ok(axum::extract::ws::Message::Pong(data))),
                Ok(Message::Close(_)) => Some(Ok(axum::extract::ws::Message::Close(None))),
                Ok(_) => None,
                Err(_) => None,
            }
        })
        .forward(client_sink);

    tokio::select! {
        _ = client_to_target => {},
        _ = target_to_client => {},
    }

    info!("WS Proxy: connection closed");
}

/// Virtual file system handler — /@fs/<path> serves files from absolute paths
/// This mirrors Vite's /@fs/ virtual module system for internal module resolution
async fn virtual_fs_handler(
    State(state): State<Arc<DevServerState>>,
    Path(path): Path<String>,
) -> Response {
    // /@fs/ serves files from absolute paths on the filesystem, but only
    // inside the allowed roots (project root + `PLEDGE_DEV_FS_ALLOW`), never
    // secrets (deny-list), and only module/asset extensions. The guard
    // canonicalizes (symlinks, case, 8.3 names) and rejects `::$DATA`
    // streams and trailing dots/spaces.
    let full_path = fs_path_from_request(&path);
    let canonical = match fs_guard::resolve_servable(&state.config.root, &path, &full_path) {
        Ok(c) => c,
        Err(d) => return denied_response(d),
    };

    if let Ok(content) = tokio::fs::read(&canonical).await {
        if content.len() > MAX_RESPONSE_SIZE {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
        }
        let content_type = guess_content_type(&canonical.to_string_lossy());
        return (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            content,
        )
            .into_response();
    }

    (StatusCode::NOT_FOUND, "Virtual file not found").into_response()
}

/// Virtual ID handler — /@id/<id> serves modules by virtual identifier
/// Used for resolving bare module specifiers and internal module IDs
async fn virtual_id_handler(
    State(state): State<Arc<DevServerState>>,
    Path(path): Path<String>,
) -> Response {
    // /@id/ resolves virtual module IDs to actual files.
    //
    // SECURITY: every branch below joins `path` (attacker-controlled — it's
    // the raw URL path segment) onto `state.config.root`. Unlike
    // `virtual_fs_handler` above, this function previously never checked
    // the joined path stayed within the project root before reading it —
    // a request like `/@id/../../../../etc/passwd` (or its `..%2f`-encoded
    // form, which axum's `Path` extractor already decodes before this
    // function sees it) would `.join()` straight through the `..`
    // components and read arbitrary files outside the project, served back
    // to the requester. Fixed by checking `is_path_within` (the same guard
    // `public_dir_handler` already uses below) before every read. See
    // PRODUCTION-READINESS-100.md goal 77 — found while writing adversarial
    // tests for this handler, not merely confirming an existing guard.
    //
    // Every branch is additionally gated by `fs_guard::resolve_servable`
    // (deny-list for secrets, extension allowlist, symlink/short-name
    // resolution), applied to the file actually read.
    //
    // Plugin virtual modules: `resolveId` then `load` (then the chained
    // `transform`s), before any file-system lookup. Vite escapes the NUL that
    // prefixes virtual ids as `__x00__` in URLs.
    if let Some(hooks) = state.plugin_hooks.as_ref() {
        let requested = path.replace("__x00__", "\0");
        let id = match hooks.resolve_id(&requested, None).await {
            Ok(Some(r)) if r.external => None,
            Ok(Some(r)) => Some(r.id),
            Ok(None) => Some(requested.clone()),
            Err(e) => return plugin_hook_error_response(&state, &path, &requested, &e),
        };
        if let Some(id) = id {
            match hooks.load(&id).await {
                Ok(Some(loaded)) => {
                    let (code, map) =
                        match run_plugin_transform(hooks, &id, loaded.code, loaded.map).await {
                            Ok(v) => v,
                            Err(e) => return plugin_hook_error_response(&state, &path, &id, &e),
                        };
                    if code.len() > MAX_RESPONSE_SIZE {
                        return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large")
                            .into_response();
                    }
                    let rewritten = rewrite_imports(
                        &code,
                        &path,
                        &state.config.resolve_alias,
                        &state.prebundled_deps,
                        &state.config.root,
                    );
                    return serve_js_module(&path, &rewritten, map.as_deref(), &state).await;
                }
                Ok(None) => {}
                Err(e) => return plugin_hook_error_response(&state, &path, &id, &e),
            }
        }
    }

    // First try as a bare specifier in node_modules
    let node_modules_path = state.config.root.join("node_modules").join(&path);
    if node_modules_path.is_file()
        && is_path_within(&node_modules_path, &state.config.root)
        && let Ok(guarded) =
            fs_guard::resolve_servable(&state.config.root, &path, &node_modules_path)
        && let Ok(content) = tokio::fs::read(&guarded).await
    {
        if content.len() > MAX_RESPONSE_SIZE {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
        }
        let content_type = guess_content_type(&path);
        return (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            content,
        )
            .into_response();
    }

    // Try as a path relative to project root
    let root_path = state.config.root.join(&path);
    if root_path.exists()
        && root_path.is_file()
        && is_path_within(&root_path, &state.config.root)
        && let Ok(guarded) = fs_guard::resolve_servable(&state.config.root, &path, &root_path)
        && let Ok(content) = tokio::fs::read(&guarded).await
    {
        if content.len() > MAX_RESPONSE_SIZE {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
        }
        let content_type = guess_content_type(&path);
        return (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            content,
        )
            .into_response();
    }

    // Try resolving as a package with module/main field
    let pkg_json = state
        .config
        .root
        .join("node_modules")
        .join(&path)
        .join("package.json");
    if pkg_json.exists()
        && is_path_within(&pkg_json, &state.config.root)
        && let Ok(content) = std::fs::read_to_string(&pkg_json)
        && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
    {
        let entry = pkg
            .get("module")
            .or_else(|| pkg.get("main"))
            .and_then(|v| v.as_str())
            .unwrap_or("index.js");
        let entry_path = state
            .config
            .root
            .join("node_modules")
            .join(&path)
            .join(entry);
        if entry_path.is_file()
            && is_path_within(&entry_path, &state.config.root)
            && let Ok(guarded) = fs_guard::resolve_servable(&state.config.root, &path, &entry_path)
            && let Ok(entry_content) = tokio::fs::read(&guarded).await
        {
            if entry_content.len() > MAX_RESPONSE_SIZE {
                return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
            }
            return (
                [
                    (
                        header::CONTENT_TYPE,
                        "application/javascript; charset=utf-8",
                    ),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                entry_content,
            )
                .into_response();
        }
    }

    (StatusCode::NOT_FOUND, "Virtual module not found").into_response()
}

/// Public directory handler — serves static assets from the configured public directory
async fn public_dir_handler(
    State(state): State<Arc<DevServerState>>,
    Path(path): Path<String>,
    request_headers: HeaderMap,
) -> Response {
    let public_dir = &state.config.dev_server.public_dir;
    let public_path = state.config.root.join(public_dir).join(&path);

    // Security: prevent path traversal via `../` in the request path
    if !is_path_within(&public_path, &state.config.root) {
        return (StatusCode::FORBIDDEN, "Path traversal denied").into_response();
    }

    if !public_path.is_file() {
        return (StatusCode::NOT_FOUND, "Static asset not found").into_response();
    }
    let public_path = match fs_guard::resolve_servable(&state.config.root, &path, &public_path) {
        Ok(p) => p,
        Err(d) => return denied_response(d),
    };

    if let Ok(content) = tokio::fs::read(&public_path).await {
        if content.len() > MAX_RESPONSE_SIZE {
            return (StatusCode::PAYLOAD_TOO_LARGE, "Response too large").into_response();
        }
        let content_type = guess_content_type(&path);

        // Compute ETag from content hash for conditional requests
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        let etag = format!("\"{:x}\"", hasher.finish());

        // Check If-None-Match for conditional request — return 304 if ETag matches
        if let Some(if_none_match) = request_headers.get("if-none-match")
            && if_none_match == etag.as_bytes()
        {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag.as_str())]).into_response();
        }

        return (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "public, max-age=3600"),
                (header::ETAG, etag.as_str()),
            ],
            content,
        )
            .into_response();
    }

    (StatusCode::NOT_FOUND, "Static asset not found").into_response()
}

/// Guess content type from file extension for static asset serving
fn guess_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Open the default browser to the given URL.
/// Uses the `opener` crate for cross-platform support (Windows, macOS, Linux, WSL).
fn open_browser(url: &str) {
    match opener::open(url) {
        Ok(_) => info!("Opened browser at {}", url),
        Err(e) => tracing::warn!("Failed to open browser: {}", e),
    }
}

/// Generate a self-signed TLS certificate and private key for local HTTPS dev server.
/// Uses rcgen to create a certificate valid for localhost and the local IP.
fn generate_self_signed_cert(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> anyhow::Result<()> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    let mut san_names = vec!["localhost".to_string()];
    if let Ok(ip) = local_ip_address::local_ip() {
        san_names.push(ip.to_string());
    }

    let mut params = CertificateParams::new(san_names.clone())?;

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "PledgePack Dev Server");
    dn.push(DnType::OrganizationName, "PledgePack");
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;

    Ok(())
}

#[cfg(test)]
mod hot_update_plan_tests {
    use super::*;
    use pledgepack_js_plugin_host::HotUpdateResult;

    #[test]
    fn no_plugin_claim_means_default_update() {
        assert_eq!(plan_hot_update(None), HotUpdatePlan::Default);
    }

    #[test]
    fn empty_module_list_suppresses_the_update() {
        let r = HotUpdateResult { module_ids: vec![] };
        assert_eq!(plan_hot_update(Some(r)), HotUpdatePlan::Suppress);
    }

    #[test]
    fn explicit_module_list_replaces_the_default_set() {
        let r = HotUpdateResult {
            module_ids: vec!["src/a.ts".into(), "src/b.ts".into()],
        };
        assert_eq!(
            plan_hot_update(Some(r)),
            HotUpdatePlan::Modules(vec!["src/a.ts".into(), "src/b.ts".into()])
        );
    }
}

#[cfg(test)]
mod review_regression_tests {
    use super::*;
    use tower::ServiceExt;

    #[test]
    fn constant_time_eq_matches_only_identical_slices() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn extract_imports_survives_multibyte_char_after_dynamic_import_paren() {
        // `search_from = after_pattern + 1` used to land mid-character.
        let imports = extract_imports("const m = await import(\u{e9}\u{e9}); import x from 'a';");
        assert!(imports.contains(&"a".to_string()));
    }

    #[test]
    fn rewrite_imports_survives_multibyte_char_after_dynamic_import_paren() {
        let out = rewrite_imports(
            "import(\u{6a21}\u{5757}); foo('./bar');",
            "src/main.ts",
            &[],
            &std::collections::HashMap::new(),
            std::path::Path::new("."),
        );
        // The unrelated string literal must not be treated as the import's
        // specifier.
        assert!(out.contains("foo('./bar')"), "got: {out}");
    }

    #[test]
    fn transform_error_module_escapes_quotes_and_newlines() {
        let js = transform_error_module("src/a'b.ts", "it's \"bad\"\n');alert(1);//");
        // banner line + console.error + throw = exactly 3 lines: the newline
        // in the message must be escaped, not emitted raw.
        assert_eq!(js.lines().count(), 3, "got: {js}");
        assert!(js.contains("\n"));
        let quoted = serde_json::to_string(
            "it's \"bad\"
');alert(1);//",
        )
        .unwrap();
        assert!(js.contains(&quoted), "message not JSON-encoded in: {js}");
    }

    #[test]
    fn fs_path_from_request_keeps_windows_drive_paths() {
        assert_eq!(
            fs_path_from_request("C:/proj/src/a.js"),
            std::path::PathBuf::from("C:/proj/src/a.js")
        );
        assert_eq!(
            fs_path_from_request("home/u/a.js"),
            std::path::PathBuf::from("/home/u/a.js")
        );
    }

    #[test]
    fn proxy_upstream_url_forwards_query_and_honours_rewrite() {
        assert_eq!(
            proxy_upstream_url("http://b:1/", "/api", false, "/api/users", Some("id=1&x=2")),
            "http://b:1/api/users?id=1&x=2"
        );
        assert_eq!(
            proxy_upstream_url("http://b:1", "/api", true, "/api/users", None),
            "http://b:1/users"
        );
        assert_eq!(normalize_proxy_prefix("api/"), Some("/api".to_string()));
        assert_eq!(normalize_proxy_prefix("/"), None);
    }

    #[tokio::test]
    async fn proxy_routes_build_and_forward_query_headers_and_cookies() {
        ensure_crypto_provider();
        // Backend echoing what it received, and setting two cookies.
        let backend = Router::new().route(
            "/api/echo",
            axum::routing::post(
                |headers: HeaderMap, uri: axum::http::Uri, body: String| async move {
                    let mut resp = Response::new(axum::body::Body::from(format!(
                        "{}|{}|{}|{}",
                        uri.query().unwrap_or(""),
                        headers
                            .get("x-custom")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or(""),
                        headers
                            .get("x-added")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or(""),
                        body
                    )));
                    resp.headers_mut()
                        .append(header::SET_COOKIE, HeaderValue::from_static("a=1"));
                    resp.headers_mut()
                        .append(header::SET_COOKIE, HeaderValue::from_static("b=2"));
                    resp
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, backend).await.ok();
        });

        let cfg = pledgepack_core::config::ProxyConfig {
            path: "/api".into(),
            target: format!("http://{addr}"),
            rewrite: false,
            headers: [("x-added".to_string(), "yes".to_string())].into(),
            ws: true,
        };
        // Panicked at construction with axum 0.8 (`/*rest` syntax, and a
        // second handler on the same path when `ws` was set).
        let app = add_proxy_routes(Router::new(), &[cfg]);

        let resp = app
            .oneshot(
                axum::http::Request::post("/api/echo?id=7")
                    .header("x-custom", "hello")
                    .body(axum::body::Body::from("payload"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get_all(header::SET_COOKIE).iter().count(), 2);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"id=7|hello|yes|payload");
    }
}

#[cfg(test)]
mod bind_and_banner_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    fn sa(ip: IpAddr, port: u16) -> SocketAddr {
        SocketAddr::new(ip, port)
    }

    #[test]
    fn network_line_only_for_non_loopback_binds() {
        let lan: Option<IpAddr> = Some("192.168.1.28".parse().unwrap());
        // localhost => 127.0.0.1 + ::1: nothing listens on the LAN address.
        let loop_both = [
            sa(Ipv4Addr::LOCALHOST.into(), 3000),
            sa(Ipv6Addr::LOCALHOST.into(), 3000),
        ];
        assert!(network_urls("http", &loop_both, lan).is_empty());
        // 0.0.0.0 => the LAN address is reachable.
        let any = [sa(Ipv4Addr::UNSPECIFIED.into(), 3000)];
        assert_eq!(
            network_urls("http", &any, lan),
            vec!["http://192.168.1.28:3000".to_string()]
        );
        // ...unless no LAN IP is known.
        assert!(network_urls("http", &any, None).is_empty());
        // A specific LAN bind advertises exactly that address.
        let one = [sa("10.0.0.5".parse().unwrap(), 8080)];
        assert_eq!(
            network_urls("https", &one, lan),
            vec!["https://10.0.0.5:8080".to_string()]
        );
    }

    #[test]
    fn urls_bracket_ipv6() {
        assert_eq!(
            url_for("http", Ipv6Addr::LOCALHOST.into(), 3000),
            "http://[::1]:3000"
        );
    }

    #[tokio::test]
    async fn localhost_binds_v4_and_v6_on_the_same_port() {
        let ls = bind_listeners("localhost", 0).await.unwrap();
        let addrs: Vec<SocketAddr> = ls.iter().map(|l| l.local_addr().unwrap()).collect();
        assert!(
            addrs
                .iter()
                .any(|a| a.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
        if std::net::TcpListener::bind("[::1]:0").is_ok() && addrs.len() > 1 {
            assert_eq!(addrs[0].port(), addrs[1].port());
            assert!(addrs[1].ip().is_loopback() && addrs[1].is_ipv6());
        }
    }

    #[tokio::test]
    async fn explicit_host_binds_only_that_address() {
        let ls = bind_listeners("127.0.0.1", 0).await.unwrap();
        assert_eq!(ls.len(), 1);
        let ls = bind_listeners("[::1]", 0).await;
        if let Ok(ls) = ls {
            assert_eq!(ls.len(), 1);
        }
    }

    #[test]
    fn module_requests_are_recognised() {
        let mut h = HeaderMap::new();
        assert!(!wants_module(None, &h));
        assert!(!wants_module(Some("t=1"), &h));
        assert!(wants_module(Some("import"), &h));
        assert!(wants_module(Some("t=1&import"), &h));
        h.insert("sec-fetch-dest", HeaderValue::from_static("script"));
        assert!(wants_module(None, &h));
        h.insert("sec-fetch-dest", HeaderValue::from_static("empty"));
        assert!(!wants_module(None, &h));
    }

    #[test]
    fn json_specifiers_are_marked_as_module_imports() {
        assert_eq!(add_js_extension("./data.json", None), "./data.json?import");
        assert_eq!(add_js_extension("./a.ts", None), "./a.ts");
        assert_eq!(add_js_extension("./a", None), "./a.tsx");
    }
}

#[cfg(test)]
mod hmr_client_tests {
    use super::*;

    #[test]
    fn hmr_client_is_a_single_balanced_script_tag() {
        // The client used to live as three diverging inline copies. It now
        // comes from `hmr_client.html` — pin the essential surface so a future
        // edit can't silently drop a capability (framework HMR, CSS updates,
        // the error overlay, WS reconnect).
        assert_eq!(HMR_CLIENT_TAG.matches("<script>").count(), 1);
        assert_eq!(HMR_CLIENT_TAG.matches("</script>").count(), 1);
        for marker in [
            "__pledge_connect_ws",
            "__pledge_hmr_modules",
            "__pledge_vue_components",
            "__pledge_svelte_components",
            "__pledge_fast_refresh",
            "__pledge_hot_accept_callbacks",
            "pledge:hmr-success",
            "showPledgeError",
            "updatePledgeCSS",
            "unhandledrejection",
            ".vue",
            ".svelte",
        ] {
            assert!(HMR_CLIENT_TAG.contains(marker), "missing {marker}");
        }
    }

    #[test]
    fn inject_hmr_client_places_script_before_body_close_once() {
        let mut html = "<html><body><div>app</div></body></html>".to_string();
        inject_hmr_client(&mut html);
        let script_at = html.find("<script>").unwrap();
        let body_close = html.find("</body>").unwrap();
        assert!(script_at < body_close);
        assert_eq!(html.matches("</body>").count(), 1);
    }

    #[test]
    fn inject_hmr_client_appends_when_no_body_tag() {
        let mut html = "<html><div>app</div></html>".to_string();
        inject_hmr_client(&mut html);
        assert!(html.contains("<script>"));
        assert!(html.ends_with("</body></html>"));
    }
}
