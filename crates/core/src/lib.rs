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
pub mod bench;
pub mod budgets;
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
pub mod doctor;
pub mod drizzle;
pub mod ecosystem;
pub mod edge;
pub mod encrypt;
pub mod engine;
pub mod env;
pub mod estree;
pub mod examples_gallery;
pub mod favicons;
pub mod fonts;
pub mod html;
pub mod i18n;
pub mod image_pipeline;
pub mod lsp_server;
pub mod migrate;
pub mod module;
pub mod module_graph;
pub mod output_distribution;
pub mod performance;
pub mod pipeline;
pub mod playground;
pub mod plugin_docs;
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
pub mod svg;
pub mod tailwind_v4;
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

/// Create a debounced file watcher using notify-debouncer.
/// Returns a receiver that yields paths of changed files (debounced).
/// This replaces manual debounce logic with the crate's built-in debouncing.
pub fn create_debounced_watcher(
    root: &std::path::Path,
    debounce_ms: u64,
) -> anyhow::Result<std::sync::mpsc::Receiver<std::path::PathBuf>> {
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

    // Intentionally leaked: the debouncer owns the underlying `notify` watcher
    // and the callback closure that feeds the channel. It must live for the
    // process lifetime to keep watching for file changes. There is no
    // owning struct returned from this function (only the `Receiver`), so the
    // debouncer has nowhere to be stored.
    //
    // This is a known minor memory leak (~1 KB per watcher). The watcher is
    // typically created once per dev server session, so the impact is negligible.
    // TODO: Return a `WatcherHandle` struct that owns the debouncer for proper
    // lifecycle management and deterministic shutdown.
    std::mem::forget(debouncer);

    Ok(rx)
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
        .unwrap_or_else(|| {
            path.strip_prefix(r"\\?\")
                .unwrap_or(path)
                .to_string()
        });
    path.replace('\\', "/")
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
