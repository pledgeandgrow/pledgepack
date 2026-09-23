//! pledge-core: The core build engine
//!
//! Orchestrates the build pipeline:
//!   1. Resolve entry point
//!   2. Parse + transform modules (via SWC)
//!   3. Build module graph (via Zig native layer)
//!   4. Cache results (function-level incremental computation)
//!   5. Output bundles (dev: serve modules, prod: optimize + chunk)

/// Schema version of the PledgeStack integration contract between
/// `pledgepack-adapter-pledgestack` (which produces `RouteManifest` and
/// stamps it into the `__pledge_ps_manifest.json` file) and
/// `pledgepack-dev-server` (which stamps the same version onto the
/// `/__pledge_router` response as the `X-Pledgepack-Schema-Version`
/// header), so both surfaces PledgeStack's `bundler-pledgepack` adapter
/// checks report the same number from one source of truth instead of two
/// independently-maintained constants drifting apart. Bump when
/// `RouteManifest`'s shape changes in a way a consumer should know about —
/// see `RouteManifest::SCHEMA_VERSION`'s doc comment in
/// `pledgepack-adapter-pledgestack` for the full versioning policy.
/// PRODUCTION-READINESS-100.md goal 81 — there was previously no version
/// field anywhere in this contract at all.
pub const PLEDGESTACK_MANIFEST_SCHEMA_VERSION: u32 = 1;

pub mod a11y;
pub mod advanced;
pub mod analyzer;
pub mod api;
pub mod asset_pipeline;
pub mod ast_pool;
pub mod bench;
pub mod budgets;
pub mod bundle;
pub mod compression;
pub mod config;
pub mod config_validate;
pub mod css_advanced;
pub mod css_features;
pub mod css_frameworks;
pub mod css_in_js;
pub mod dep_bundler;
pub mod detect;
pub mod determinism;
pub mod diagnostics;
pub mod doctor;
pub mod drizzle;
pub mod ecosystem;
pub mod edge;
pub mod encrypt;
pub mod engine;
pub mod env;
pub mod estree;
pub mod examples_gallery;
pub mod export_check;
pub mod favicons;
pub mod fonts;
pub mod html;
pub mod i18n;
pub mod image_pipeline;
pub mod js_config;
pub mod lsp_server;
pub mod migrate;
pub mod module;
pub mod module_graph;
pub mod output_distribution;
// `package_map` lives in `pledgepack-resolver` (the single resolution
// implementation); re-exported so `pledgepack_core::package_map` keeps working.
pub use pledgepack_resolver::package_map;
pub mod performance;
pub mod pipeline;
pub mod playground;
pub mod plugin_docs;
pub mod plugin_hooks;
pub mod plugin_registry;
pub mod plugin_system;
pub mod plugin_template;
pub mod plugin_types;
pub mod polyfills;
pub mod postcss;
pub mod presets;
pub mod prisma;
pub mod router;
pub mod rtl;
pub mod security;
pub mod service_worker;
pub mod sourcemap_compose;
pub mod svg;
pub mod tailwind_v4;
pub mod task_transform;
pub mod telemetry;
pub mod transform;
pub mod transform_optimizations;
pub mod type_check;
pub mod visual_regression;
pub mod webhooks;

pub use config::BuildConfig;
pub use config::CacheConfig;
pub use config::ExportsConfig;
pub use config::GraphqlConfig;
pub use config::HttpsConfig;
pub use config::ImageConfig;
pub use config::LibraryConfig;
pub use config::OptimizeConfig;
pub use config::OutputFormat;
pub use config::PathAlias;
pub use config::PledgeConfig;
pub use config::PluginPreset;
pub use config::ProxyConfig;
pub use config::RemoteCacheSettings;
pub use config::ResolveConfig;
pub use config::SecurityConfig;
pub use config::SwCacheRule;
pub use config::SwCachingConfig;
pub use config::TestConfig;
pub use config::TransformPipelineConfig;
pub use config::WatchConfig;
pub use config::WorkspaceConfig;
pub use engine::BuildEngine;
pub use engine::EmitChunk;
pub use env::EnvVars;
pub use module::{ModuleId, ModuleKind, ResolvedModule};
pub use module_graph::SerializableModuleGraph;

use pledgepack_native_sys as native;

/// Re-export the Zig-backed graph for internal use
pub use native::Graph;

/// Owns a debounced file watcher. The underlying `notify` watcher (and the
/// callback feeding the channel) lives exactly as long as this handle: dropping
/// it stops watching deterministically. Derefs to the event
/// [`Receiver`](std::sync::mpsc::Receiver), so `handle.recv()`,
/// `handle.try_recv()`, `handle.iter()` etc. work directly.
pub struct WatcherHandle {
    rx: std::sync::mpsc::Receiver<std::path::PathBuf>,
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
}

impl std::ops::Deref for WatcherHandle {
    type Target = std::sync::mpsc::Receiver<std::path::PathBuf>;
    fn deref(&self) -> &Self::Target {
        &self.rx
    }
}

/// Create a debounced file watcher using notify-debouncer.
/// Returns a [`WatcherHandle`] that yields paths of changed files (debounced)
/// and keeps the watcher alive until dropped.
pub fn create_debounced_watcher(
    root: &std::path::Path,
    debounce_ms: u64,
) -> anyhow::Result<WatcherHandle> {
    use notify::RecursiveMode;
    use notify_debouncer_full::new_debouncer;
    use std::time::Duration;

    let (tx, rx) = std::sync::mpsc::channel::<std::path::PathBuf>();

    let mut debouncer = new_debouncer(
        Duration::from_millis(debounce_ms),
        None,
        move |result: Result<Vec<notify_debouncer_full::DebouncedEvent>, Vec<notify::Error>>| {
            if let Ok(events) = result {
                for event in events {
                    for path in &event.paths {
                        let _ = tx.send(path.clone());
                    }
                }
            }
        },
    )?;

    debouncer.watch(root, RecursiveMode::Recursive)?;

    Ok(WatcherHandle {
        rx,
        _debouncer: debouncer,
    })
}

/// Format a byte count as a human-readable string using humansize.
/// Replaces the 4 duplicate `format_bytes` functions across the codebase.
pub fn format_size(bytes: usize) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}

/// Normalize a path to use forward slashes (cross-platform consistent).
/// Handles both Windows backslashes and already-normalized paths.
pub fn normalize_path(path: &std::path::Path) -> String {
    normalize_path_str(&path.to_string_lossy())
}

/// Normalize a path-like string to use forward slashes (cross-platform
/// consistent). Equivalent to [`normalize_path`] but for string inputs
/// such as URL paths and module specifiers.
///
/// Also strips Windows' `\\?\` extended-length ("verbatim") prefix — added
/// by `Path::canonicalize()` on Windows, harmless for filesystem APIs but
/// leaking into every user-facing message built from a canonicalized path
/// (`why`'s import chains, `generate-env-types`'s success message, etc.) as
/// a confusing `//?/C:/...` once backslashes are converted to slashes.
pub fn normalize_path_str(path: &str) -> String {
    let path = path
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(path).to_string());
    path.replace('\\', "/")
}

/// Strip Windows' extended-length ("verbatim") prefix from a path string for
/// display: `\\?\C:\a` -> `C:\a`, `\\?\UNC\srv\sh` -> `\\srv\sh`.
/// Other strings are returned unchanged. Backslashes are NOT converted.
pub fn strip_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
    }
}

/// Render a path for user-facing output (CLI messages, logs, errors). Internal
/// code keeps canonical paths (`canonicalize` yields `\\?\C:\...` on
/// Windows, which is right for filesystem APIs); this only drops the verbatim
/// prefix so users see `C:\...`. Unlike `{:?}` on a `Path` it also does not
/// double every backslash.
pub fn display_path(path: &std::path::Path) -> String {
    strip_verbatim_prefix(&path.to_string_lossy())
}

/// Generate a JSON Schema for `PledgeConfig`, suitable for IDE autocompletion
/// and config validation. Returns the schema as a `serde_json::Value`.
///
/// Errors during schema serialization are propagated rather than silently
/// swallowed, so callers can surface them to the user.
pub fn generate_config_schema() -> anyhow::Result<serde_json::Value> {
    let schema = schemars::schema_for!(PledgeConfig);
    serde_json::to_value(&schema).map_err(|e| {
        tracing::error!("Failed to serialize config schema: {}", e);
        anyhow::anyhow!("Config schema serialization failed: {}", e)
    })
}

#[cfg(test)]
mod watcher_handle_tests {
    use super::*;

    #[test]
    fn debounced_watcher_reports_changes_and_stops_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let watcher = create_debounced_watcher(dir.path(), 50).unwrap();
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        let changed = watcher
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("expected a change event");
        assert!(changed.to_string_lossy().contains("a.txt"), "{changed:?}");
        // Dropping the handle tears down the watcher (previously leaked via
        // `mem::forget`); the receiver-side channel is closed with it.
        drop(watcher);
    }
}

#[cfg(test)]
mod display_path_tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_stripped_for_display_only() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\C:\Users\me\proj"),
            r"C:\Users\me\proj"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\srv\share\p"),
            r"\\srv\share\p"
        );
        assert_eq!(strip_verbatim_prefix(r"C:\plain"), r"C:\plain");
        assert_eq!(strip_verbatim_prefix("/unix/path"), "/unix/path");
        assert_eq!(
            display_path(std::path::Path::new(r"\\?\C:\a\dist")),
            r"C:\a\dist"
        );
    }
}
