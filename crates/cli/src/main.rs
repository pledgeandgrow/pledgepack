// Pledge CLI — the entry point
//
// Usage:
//   pledge dev          Start dev server with HMR
//   pledge build        Production build
//   pledge serve        Preview production build
//   pledge cache clear  Clear the filesystem cache
//   pledge bench        Run benchmarks

// Global allocator — mimalloc by default for better multi-threaded performance.
// Use `--features jemalloc` for heap profiling and leak detection.
// Note: jemalloc is only available on non-Windows platforms.
#[cfg(not(all(feature = "jemalloc", not(target_os = "windows"))))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(all(feature = "jemalloc", not(target_os = "windows")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use pledgepack_adapter_next::NextAdapter;
use pledgepack_adapter_pledgestack::PledgeStackAdapter;
use pledgepack_adapter_react::ReactAdapter;
use pledgepack_adapter_solid::SolidAdapter;
use pledgepack_adapter_tanstack::TanStackAdapter;
use pledgepack_core::compression;
use pledgepack_core::config::BuildMode;
use pledgepack_core::config::Framework;
use pledgepack_core::config_validate;
use pledgepack_core::dep_bundler::DepBundler;
use pledgepack_core::detect;
use pledgepack_core::doctor;
use pledgepack_core::edge;
use pledgepack_core::env::EnvVars;
use pledgepack_core::html;
use pledgepack_core::migrate;
use pledgepack_core::{BuildEngine, PledgeConfig};
use pledgepack_js_plugin_host::JsPluginHost;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// Centralized framework dependency versions for project templates.
///
/// Keeping these in one place makes it trivial to bump versions across all
/// templates (cached and from-scratch) without hunting through string literals.
mod template_versions {
    /// React + ReactDOM (used by pledge, react, next, and tanstack templates)
    pub const REACT: &str = "^19.0.0";
    /// SolidJS (used by the solid template)
    pub const SOLID: &str = "^1.8.0";
    /// Vue 3 (used by the vue template)
    pub const VUE: &str = "^3.4.0";
    /// Svelte (used by the svelte template)
    pub const SVELTE: &str = "^4.2.0";

    // CSS framework devDependencies
    pub const TAILWINDCSS: &str = "^3.4.0";
    pub const POSTCSS: &str = "^8.4.0";
    pub const AUTOPREFIXER: &str = "^10.4.0";
    pub const UNOCSS: &str = "^0.58.0";
    pub const PANDA_CSS: &str = "^0.30.0";
    pub const VANILLA_EXTRACT: &str = "^1.14.0";
}

/// Build the framework dependencies JSON object for a given template name.
fn framework_deps(template: &str) -> serde_json::Value {
    match template {
        "pledge" | "react" | "next" | "tanstack" => serde_json::json!({
            "react": template_versions::REACT,
            "react-dom": template_versions::REACT
        }),
        "solid" => serde_json::json!({
            "solid-js": template_versions::SOLID
        }),
        "vue" => serde_json::json!({
            "vue": template_versions::VUE
        }),
        "svelte" => serde_json::json!({
            "svelte": template_versions::SVELTE
        }),
        _ => serde_json::json!({}),
    }
}

/// Build the CSS framework devDependencies JSON object for a given CSS choice.
fn css_framework_dev_deps(css: &str) -> serde_json::Value {
    match css {
        "tailwind" => serde_json::json!({
            "tailwindcss": template_versions::TAILWINDCSS,
            "postcss": template_versions::POSTCSS,
            "autoprefixer": template_versions::AUTOPREFIXER
        }),
        "unocss" => serde_json::json!({
            "unocss": template_versions::UNOCSS
        }),
        "panda-css" => serde_json::json!({
            "@pandacss/dev": template_versions::PANDA_CSS
        }),
        "vanilla-extract" => serde_json::json!({
            "@vanilla-extract/css": template_versions::VANILLA_EXTRACT
        }),
        _ => serde_json::json!({}),
    }
}

#[derive(Parser)]
#[command(
    name = "pledge",
    version,
    about = "A Rust+Zig bundler with incremental computation, JS plugins, and Rollup-quality output"
)]
struct Cli {
    /// Project root directory (default: current directory)
    #[arg(long, global = true)]
    root: Option<Utf8PathBuf>,

    /// Config file path (default: auto-detect pledge.json)
    #[arg(long, global = true)]
    config: Option<Utf8PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the dev server with HMR
    Dev {
        /// Port to serve on
        #[arg(short, long)]
        port: Option<u16>,

        /// Host to bind to
        #[arg(long)]
        host: Option<String>,

        /// Open browser on start
        #[arg(long)]
        open: bool,

        /// Enable HTTPS dev server (auto-generates self-signed certs if not provided)
        #[arg(long)]
        https: bool,

        /// Unix socket path to listen on (Unix only, overrides --port/--host)
        #[arg(long)]
        socket: Option<String>,
    },

    /// Create a production build
    Build {
        /// Output directory
        #[arg(short, long)]
        out_dir: Option<Utf8PathBuf>,

        /// Disable source maps
        #[arg(long)]
        no_sourcemap: bool,

        /// Enable build profiling (timing per phase)
        #[arg(long)]
        profile: bool,

        /// Watch mode — rebuild on file change
        #[arg(long)]
        watch: bool,

        /// Verify build output integrity after emit
        #[arg(long)]
        verify: bool,

        /// Run TypeScript type checking during build (#71)
        #[arg(long)]
        type_check: bool,

        /// Check bundle size budgets and exit non-zero on violations (#102)
        #[arg(long)]
        check_budgets: bool,

        /// Environment-specific build (#117)
        /// Loads .env.{env} and sets process.env.NODE_ENV accordingly
        #[arg(long)]
        env: Option<String>,

        /// GraphQL code generation (#116)
        /// Generates TypeScript types from .graphql schema files
        #[arg(long)]
        codegen: bool,
    },

    /// Preview a production build with compression
    Preview {
        /// Port to serve on
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Serve a production build (alias for preview)
    Serve {
        /// Port to serve on
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Scaffold a new project (defaults to PledgeStack if no template given)
    Create {
        /// Template name (pledge, react, vue, svelte, solid, next, tanstack, vanilla)
        /// If omitted, defaults to pledge
        template: Option<String>,

        /// Project name / directory
        name: Option<String>,

        /// Flash create — skip wizard, git init, README; just get coding
        #[arg(long)]
        flash: bool,
    },

    /// Add Pledgepack to an existing project (migrates config from Vite/webpack/CRA)
    Init {
        /// Force overwrite existing pledge.config.ts
        #[arg(long)]
        force: bool,

        /// Skip framework detection, use this framework
        #[arg(long)]
        framework: Option<String>,
    },

    /// Diagnose build issues, config problems, and performance bottlenecks
    Doctor,

    /// Migrate config from Vite/webpack/CRA/Next.js to pledge.config.ts
    Migrate {
        /// Dry run — show what would be migrated without writing files
        #[arg(long)]
        dry_run: bool,
    },

    /// Validate config and show field suggestions
    Config,

    /// Analyze bundle size with interactive treemap
    Analyze {
        /// Port to serve the analysis on
        #[arg(short, long)]
        port: Option<u16>,

        /// Generate interactive dependency graph instead of treemap (#104)
        #[arg(long)]
        graph: bool,
    },

    /// Run tests (Vitest-compatible API)
    Test {
        /// Test file pattern (glob)
        #[arg(short, long)]
        pattern: Option<String>,

        /// Watch mode
        #[arg(short, long)]
        watch: bool,

        /// UI mode (browser)
        #[arg(long)]
        ui: bool,

        /// Visual regression testing — screenshot comparison (#75)
        #[arg(long)]
        visual: bool,

        /// Update visual baselines instead of comparing
        #[arg(long)]
        update_baselines: bool,
    },

    /// Manage the cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Run benchmarks
    Bench {
        /// Compare against a baseline ref (git commit hash or name) (#103)
        #[arg(long)]
        baseline: Option<String>,

        /// Regression threshold percentage (default: 10.0) (#103)
        #[arg(long)]
        threshold: Option<f64>,
    },

    /// Serve the build telemetry dashboard (#101)
    Dashboard {
        /// Port to serve the dashboard on
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Generate TypeScript declarations for env variables
    GenerateEnvTypes,

    /// Generate shell completion scripts
    Completions {
        /// Shell to generate completions for
        #[arg(short, long)]
        shell: Option<String>,
    },

    /// Generate man pages for package managers (Homebrew, Scoop, etc.)
    Manpages {
        /// Output directory for man page files
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Generate JSON Schema for pledge.config.ts / pledge.json
    /// Useful for IDE autocompletion and config validation
    Schema {
        /// Output file path (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Open the PledgePack Playground — interactive transform REPL (#66)
    Playground {
        /// Port to serve the playground on
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Manage PledgePack plugins — search, install, list, docs (#67, #68, #69)
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },

    /// Analyze why a module is included in the bundle (#82)
    Why {
        /// Module name or path to trace
        module: String,
    },

    /// Clean build artifacts and caches
    Clean {
        /// Also remove node_modules/.cache
        #[arg(long)]
        deep: bool,
    },

    /// Self-update PledgePack to the latest (or a specific) version
    Update {
        /// Version to update to (default: latest)
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginAction {
    /// Search the npm registry for PledgePack plugins
    Search {
        /// Search query (optional)
        query: Option<String>,
    },

    /// Install a plugin from npm
    Install {
        /// Plugin package name
        name: String,

        /// Install as dev dependency
        #[arg(long)]
        dev: bool,
    },

    /// List installed PledgePack plugins
    List,

    /// Scaffold a new plugin project (#68)
    Create {
        /// Plugin name
        name: String,

        /// Plugin description
        #[arg(long)]
        description: Option<String>,

        /// Author name
        #[arg(long)]
        author: Option<String>,
    },

    /// Generate API docs from a plugin source file (#69)
    Docs {
        /// Path to the plugin source file (index.js or index.ts)
        file: PathBuf,

        /// Output file path (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Clear the filesystem cache
    Clear,
    /// Show cache statistics
    Stats,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install miette's graphical error handler for rich diagnostics
    miette::set_hook(Box::new(|_| {
        Box::new(miette::MietteHandlerOpts::new().build())
    }))
    .ok();

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let root = cli.root.unwrap_or_else(|| Utf8PathBuf::from("."));
    let root_path = root.as_std_path().to_path_buf();
    let mut config = if let Some(config_path) = cli.config {
        let content = std::fs::read_to_string(&config_path)?;
        // Use the TS-aware config loader for .ts/.js/.mjs files,
        // and serde_json for .json files.
        let config_str = config_path.as_str();
        if config_str.ends_with(".ts")
            || config_str.ends_with(".js")
            || config_str.ends_with(".mjs")
            || config_str.ends_with(".cjs")
        {
            PledgeConfig::parse_ts_config(&content)?
        } else {
            serde_json::from_str(&content)?
        }
    } else {
        PledgeConfig::load(&root_path)?
    };
    config.root = std::fs::canonicalize(&root_path).unwrap_or(root_path);
    config.normalize();

    // Feature 94: Apply plugin presets
    if !config.presets.is_empty() {
        pledgepack_core::ecosystem::apply_presets(&mut config)?;
        tracing::info!("Applied {} presets", config.presets.len());
    }

    // Feature 98: Detect workspace for monorepo support
    if let Some(ref ws_config) = config.workspaces
        && ws_config.enabled
        && let Some(ws) = pledgepack_core::ecosystem::detect_workspace(&config.root)
    {
        tracing::info!(
            "Workspace detected: {} packages ({})",
            ws.packages.len(),
            ws.package_manager
        );

        // Feature 100: Shared cache at workspace root
        if ws_config.shared_cache {
            let shared_cache = pledgepack_core::ecosystem::resolve_shared_cache_dir(&ws, ws_config);
            config.cache.dir = shared_cache;
            tracing::info!("Shared cache: {}", config.cache.dir.display());
        }
    }

    match cli.command {
        Commands::Dev {
            port,
            host,
            open,
            https,
            socket,
        } => {
            if let Some(p) = port {
                config.dev_server.port = p;
            }
            if let Some(h) = host {
                config.dev_server.host = h;
            }
            if open {
                config.dev_server.open = true;
            }
            if https && config.https.is_none() {
                let cert_dir = std::env::temp_dir().join("pledgepack-certs");
                std::fs::create_dir_all(&cert_dir).ok();
                config.https = Some(pledgepack_core::config::HttpsConfig {
                    cert: cert_dir.join("dev-cert.pem"),
                    key: cert_dir.join("dev-key.pem"),
                });
            }
            if let Some(ref sock) = socket {
                config.dev_server.unix_socket = Some(sock.clone());
            }
            config.mode = pledgepack_core::config::BuildMode::Development;

            let protocol = if config.https.is_some() {
                "https"
            } else {
                "http"
            };
            println!(
                "\n  \x1b[36mpledge\x1b[0m dev server starting...\n  \x1b[90m→\x1b[0m {}://{}:{}\n",
                protocol, config.dev_server.host, config.dev_server.port
            );

            let engine = BuildEngine::new(Arc::new(config.clone()));
            pledgepack_dev_server::serve(engine, &config).await?;
        }

        Commands::Build {
            out_dir,
            no_sourcemap,
            profile,
            watch,
            verify,
            check_budgets,
            env,
            codegen,
            type_check,
        } => {
            if let Some(out) = out_dir {
                config.out_dir = out.into_std_path_buf();
            }
            if no_sourcemap {
                config.source_maps = false;
            }
            if profile {
                config.profile = true;
            }
            if verify {
                config.build.verify_output = true;
            }
            if type_check {
                config.build.type_check = true;
            }
            if check_budgets {
                config.budgets.enabled = true;
            }
            config.mode = pledgepack_core::config::BuildMode::Production;

            // Feature 117: Environment-specific builds
            if let Some(ref env_name) = env {
                let env_vars = pledgepack_core::advanced::load_env_file(&config.root, env_name);
                let node_env = pledgepack_core::advanced::resolve_node_env(&env_vars, true);
                println!(
                    "  \x1b[90m→\x1b[0m Environment: {} (NODE_ENV={})",
                    env_name, node_env
                );
                // Apply env vars to config define
                for (key, val) in &env_vars {
                    config
                        .define
                        .insert(format!("process.env.{}", key), format!("\"{}\"", val));
                }
                config.define.insert(
                    "process.env.NODE_ENV".to_string(),
                    format!("\"{}\"", node_env),
                );
            }

            // Feature 116: GraphQL code generation
            if codegen {
                if let Some(ref gql_cfg) = config.graphql {
                    if !gql_cfg.schema.is_empty() {
                        let schema_path = config.root.join(&gql_cfg.schema);
                        if schema_path.exists() {
                            let schema_source = std::fs::read_to_string(&schema_path)?;
                            let codegen_cfg = pledgepack_core::advanced::GraphqlCodegenConfig {
                                schema: gql_cfg.schema.clone(),
                                output: gql_cfg.output.clone(),
                                typescript: true,
                                react_hooks: gql_cfg.react_hooks,
                            };
                            let types = pledgepack_core::advanced::generate_graphql_types(
                                &schema_source,
                                &codegen_cfg,
                            )?;
                            let out_dir = config.root.join(&gql_cfg.output);
                            std::fs::create_dir_all(&out_dir)?;
                            std::fs::write(out_dir.join("graphql-types.ts"), &types)?;
                            println!(
                                "  \x1b[32m✓\x1b[0m GraphQL types generated → {}/graphql-types.ts",
                                gql_cfg.output
                            );
                        } else {
                            println!(
                                "  \x1b[33m⚠\x1b[0m GraphQL schema not found: {}",
                                schema_path.display()
                            );
                        }
                    } else {
                        println!("  \x1b[33m⚠\x1b[0m GraphQL schema path not configured");
                    }
                } else {
                    println!(
                        "  \x1b[33m⚠\x1b[0m GraphQL config not set — add `graphql: {{ schema: 'schema.graphql' }}` to pledge.config.ts"
                    );
                }
            }

            // Feature 115: Module federation bootstrap
            if let Some(ref fed_cfg) = config.federation
                && let Some(parsed) = pledgepack_core::advanced::parse_federation_config(fed_cfg)
            {
                let bootstrap =
                    pledgepack_core::advanced::generate_federation_host_bootstrap(&parsed);
                let bootstrap_path = config.out_dir.join("__pledge_federation__.js");
                // Write after build — store for now
                std::fs::create_dir_all(&config.out_dir)?;
                std::fs::write(&bootstrap_path, &bootstrap)?;
                tracing::info!(
                    "Module federation: host '{}' with {} remotes",
                    parsed.name,
                    parsed.remotes.len()
                );
            }

            println!("\n  \x1b[36mpledge\x1b[0m building for production...\n");

            use indicatif::{ProgressBar, ProgressStyle};
            let pb = ProgressBar::new(4);
            let style = ProgressStyle::with_template(
                "  {spinner:.green} {msg} [{bar:30.cyan/blue}] {pos}/{len}",
            )
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("█░");
            pb.set_style(style);

            let profile_start = std::time::Instant::now();

            pb.set_message("Building modules");
            let mut engine = BuildEngine::new(Arc::new(config.clone()));
            let result = engine.build().await?;
            pb.inc(1);

            // #71: Type checking during build
            if config.build.type_check {
                pb.set_message("Type checking");
                let tc_result = pledgepack_core::type_check::run_type_check(&config.root)?;
                if !tc_result.success {
                    pb.finish_and_clear();
                    println!("\n  \x1b[31m✗ Type check failed:\x1b[0m\n");
                    print!(
                        "{}",
                        pledgepack_core::type_check::format_type_check_result(&tc_result)
                    );
                    anyhow::bail!("Type check failed with {} error(s)", tc_result.errors.len());
                } else {
                    println!(
                        "  \x1b[32m✓\x1b[0m {}",
                        pledgepack_core::type_check::format_type_check_result(&tc_result)
                    );
                }
            }

            pb.set_message("Optimizing chunks");
            let optimize_start = std::time::Instant::now();
            // Run optimizer (tree shaking, code splitting, vendor/shared chunks)
            // Use optimize_with_config for manual_chunks and inline_dynamic_imports support
            let entry_ids = engine.entry_ids();
            let mut optimizer = pledgepack_optimizer::Optimizer::new();
            let chunks = {
                let graph_guard = engine.graph();
                optimizer.optimize_with_config(
                    &entry_ids,
                    engine.modules(),
                    &graph_guard,
                    &config.build,
                )?
            };
            tracing::info!("Optimizer: {} chunks", chunks.len());
            let optimize_ms = optimize_start.elapsed().as_millis();
            pb.inc(1);

            pb.set_message("Emitting output");
            let emit_start = std::time::Instant::now();
            // Convert optimizer chunks to core EmitChunk and emit as chunk files
            // instead of per-module files. This wires the optimizer's code-splitting
            // decisions (vendor/shared/entry chunks) into the final output.
            let emit_chunks: Vec<pledgepack_core::engine::EmitChunk> = chunks
                .iter()
                .map(|c| pledgepack_core::engine::EmitChunk {
                    id: c.id.clone(),
                    modules: c.modules.clone(),
                    is_entry: c.chunk_type == pledgepack_optimizer::ChunkType::Entry,
                })
                .collect();
            engine.emit_with_chunks(&emit_chunks)?;
            let emit_ms = emit_start.elapsed().as_millis();
            pb.inc(1);

            // Run framework adapter for route generation, SSR manifests, and full-stack features
            let out_dir_full = config.root.join(&config.out_dir);
            match config.framework {
                Framework::Next => {
                    pb.set_message("Generating Next.js routes");
                    let mut next_adapter = NextAdapter::new(&config.root);
                    next_adapter.discover_routes()?;
                    let router_code = next_adapter.generate_router_code();
                    let router_path = out_dir_full.join("__pledge_next_router.js");
                    std::fs::create_dir_all(&out_dir_full)?;
                    std::fs::write(&router_path, &router_code)?;
                    let ssr_manifest = next_adapter.generate_ssr_manifest();
                    let manifest_path = out_dir_full.join("__pledge_next_ssr_manifest.json");
                    std::fs::write(&manifest_path, &ssr_manifest)?;
                    tracing::info!(
                        "Next.js adapter: {} routes, SSR manifest generated",
                        next_adapter.routes.len()
                    );
                    pb.inc(1);
                }
                Framework::TanStack => {
                    pb.set_message("Generating TanStack routes");
                    let mut tanstack_adapter = TanStackAdapter::new(&config.root);
                    tanstack_adapter.discover_routes()?;
                    let route_tree = tanstack_adapter.generate_route_tree();
                    let route_path = out_dir_full.join("__pledge_tanstack_route_tree.js");
                    std::fs::create_dir_all(&out_dir_full)?;
                    std::fs::write(&route_path, &route_tree)?;
                    let route_manifest = tanstack_adapter.generate_route_manifest();
                    let manifest_path = out_dir_full.join("__pledge_tanstack_manifest.json");
                    std::fs::write(&manifest_path, &route_manifest)?;
                    tracing::info!(
                        "TanStack adapter: {} routes, manifest generated",
                        tanstack_adapter.routes.len()
                    );
                    pb.inc(1);
                }
                Framework::Pledge => {
                    pb.set_message("Generating PledgeStack routes");
                    let mut ps_adapter = PledgeStackAdapter::new(&config.root);
                    ps_adapter.discover()?;
                    std::fs::create_dir_all(&out_dir_full)?;
                    // Write route manifest (frontend + API + backend + middleware)
                    let manifest_path = out_dir_full.join("__pledge_ps_manifest.json");
                    ps_adapter.write_manifest(&manifest_path)?;
                    // Prepare .psx files for Rust compilation
                    let copied = ps_adapter.prepare_psx_files(&out_dir_full)?;
                    if !copied.is_empty() {
                        tracing::info!(
                            "PledgeStack: copied {} .psx files for compilation",
                            copied.len()
                        );
                    }
                    let manifest = ps_adapter.manifest();
                    tracing::info!(
                        "PledgeStack adapter: {} frontend routes, {} API routes, {} backend routes",
                        manifest.frontend.len(),
                        manifest.api.len(),
                        manifest.backend.len()
                    );
                    pb.inc(1);
                }
                Framework::Solid => {
                    // Solid adapter transform is handled inline by core transform.rs
                    // The adapter is available for advanced HMR boundary detection if needed
                    let _solid = SolidAdapter::new();
                }
                Framework::React => {
                    // React adapter transform is handled inline by core transform.rs
                    // The adapter is available for advanced Fast Refresh boundary detection if needed
                    let _react = ReactAdapter::new();
                }
                _ => {}
            }

            // Generate env type declarations if enabled
            if config.env_dts {
                let env = EnvVars::load(&config.root, BuildMode::Production, &config.env_prefix);
                let dts = env.generate_dts(&config.env_prefix);
                let dts_path = config.root.join("pledge-env.d.ts");
                std::fs::write(&dts_path, &dts)?;
            }

            // Process HTML entry point if it exists — supports multiple script entries
            // Convention: auto-detect from app/ → src/app/ → src/ → root
            let html_path = if let Some(entry) = &config.html_entry {
                config.root.join(entry)
            } else {
                let base = config.resolve_base_dir();
                match &base {
                    Some(b) => config.root.join(b).join("index.html"),
                    None => config.root.join("index.html"),
                }
            };
            if html_path.exists() {
                let html_entry = html::process_html(&html_path)?;
                tracing::info!(
                    "HTML entry: {} scripts, {} stylesheets, {} preloads",
                    html_entry.scripts.len(),
                    html_entry.stylesheets.len(),
                    html_entry.module_preloads.len()
                );

                // Add HTML script entries to config so the engine processes them
                for script in &html_entry.scripts {
                    let script_path = script.trim_start_matches("./").trim_start_matches("/");
                    // Resolve relative to the HTML file's directory
                    let resolved =
                        if script_path.starts_with("http") || script_path.starts_with("//") {
                            continue; // Skip external URLs
                        } else {
                            let base = config.resolve_base_dir().unwrap_or_default();

                            if base.is_empty() {
                                script_path.to_string()
                            } else {
                                format!("{}/{}", base, script_path)
                            }
                        };
                    if !config.entry.contains(&resolved) {
                        config.entry.push(resolved);
                    }
                }
                tracing::info!("Entry points: {:?}", config.entry);
            }

            // File-based routing: scan app/ directory if auto-detected or configured
            // Generate __pledge_router with build-relative imports so it resolves in dist/
            if let Some(app_dir) = config.resolve_app_dir() {
                let route_table = pledgepack_core::router::scan_app_dir(&config.root, &app_dir)?;
                if !route_table.routes.is_empty() {
                    tracing::info!(
                        "App router: {} routes from {}/",
                        route_table.routes.len(),
                        app_dir
                    );
                    for route in &route_table.routes {
                        tracing::info!("  {} → {}", route.pattern, route.file);
                    }
                    // Generate virtual router module with relative imports for build output
                    let out_dir_full = config.root.join(&config.out_dir);
                    // Calculate relative prefix from out_dir back to project root
                    let depth = config.out_dir.components().count();
                    let prefix = "../".repeat(depth);
                    let router_module = route_table.generate_router_module_build(&prefix);
                    let router_path = out_dir_full.join("__pledge_router.js");
                    std::fs::write(&router_path, &router_module)?;
                    tracing::info!("  Generated router: {}", router_path.display());
                }
            }

            // Pre-bundle dependencies
            let dep_start = std::time::Instant::now();
            pb.set_message("Pre-bundling dependencies");
            let mut dep_bundler = DepBundler::new();
            let _bundled_deps = dep_bundler.pre_bundle(&config)?;
            let dep_ms = dep_start.elapsed().as_millis();
            pb.inc(1);
            pb.finish_and_clear();

            // Load JS plugins if configured
            if !config.plugins.is_empty() {
                let mut plugin_host = JsPluginHost::new();
                plugin_host.load_plugins(&config.plugins)?;
                plugin_host.build_start();
                tracing::info!("Loaded {} JS plugins", plugin_host.plugins().len());
            }

            // Generate edge bundle if configured
            if let Some(ref edge_target) = config.edge_target
                && let Some(target) = edge::EdgeTarget::parse_target(edge_target)
            {
                let out_dir = config.root.join(&config.out_dir);
                let bundle_code = engine.collect_bundle_code();
                edge::generate_edge_bundle(target, &bundle_code, None, &out_dir)?;
            }

            // Generate compressed output if configured
            if config.compress_gzip || config.compress_brotli {
                let compress_start = std::time::Instant::now();
                let out_dir = config.root.join(&config.out_dir);
                if out_dir.exists() {
                    let stats = compression::compress_directory(
                        &out_dir,
                        config.compress_gzip,
                        config.compress_brotli,
                    )?;
                    if stats.files_compressed > 0 {
                        println!(
                            "  \x1b[32m✓\x1b[0m Compressed {} files (gzip: {:.1}KB, brotli: {:.1}KB)",
                            stats.files_compressed,
                            stats.gzipped_bytes as f64 / 1024.0,
                            stats.brotli_bytes as f64 / 1024.0,
                        );
                    }
                }
                let compress_ms = compress_start.elapsed().as_millis();
                tracing::info!("Compression: {}ms", compress_ms);
            }

            let total_ms = profile_start.elapsed().as_millis();

            // Record build telemetry (#101)
            let out_dir_full = config.root.join(&config.out_dir);
            let bundle_size: u64 = if out_dir_full.exists() {
                std::fs::read_dir(&out_dir_full)
                    .ok()
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .filter_map(|e| e.metadata().ok())
                            .map(|m| m.len())
                            .sum()
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            let _ = pledgepack_core::telemetry::record_build(
                &config.root,
                total_ms,
                result.modules_built,
                result.modules_cached,
                bundle_size as usize,
                "production",
                chunks.len(),
                true,
                None,
            );

            // Send build webhook (#105)
            if config.webhooks.enabled {
                let event = pledgepack_core::webhooks::BuildEvent {
                    event: "build.complete".to_string(),
                    success: true,
                    duration_ms: total_ms,
                    modules_built: result.modules_built,
                    modules_cached: result.modules_cached,
                    bundle_size: bundle_size as usize,
                    chunk_count: chunks.len(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    error: None,
                };
                let _ = pledgepack_core::webhooks::send_webhook(&config.webhooks, event)
                    .await
                    .map_err(|e| {
                        tracing::warn!("Webhook delivery failed: {}", e);
                    });
            }

            // Check bundle size budgets (#102)
            if config.budgets.enabled {
                let chunk_sizes: Vec<(String, usize)> = chunks
                    .iter()
                    .map(|c| {
                        let size: usize = c
                            .modules
                            .iter()
                            .filter_map(|mid| engine.modules().get(mid))
                            .map(|m| m.source.len())
                            .sum();
                        (c.id.clone(), size)
                    })
                    .collect();
                let violations = pledgepack_core::budgets::check_budgets(
                    &out_dir_full,
                    &config.budgets,
                    &chunk_sizes,
                )?;

                if !violations.is_empty() {
                    // GitHub Actions annotation format
                    if std::env::var("GITHUB_ACTIONS").is_ok() {
                        print!(
                            "{}",
                            pledgepack_core::budgets::format_github_annotations(&violations)
                        );
                    }
                    // Print violations
                    for v in &violations {
                        println!("  \x1b[31m✗ budget\x1b[0m {}", v.message);
                    }
                    anyhow::bail!("Budget check failed with {} violation(s)", violations.len());
                } else {
                    println!("  \x1b[32m✓ budgets\x1b[0m all within limits");
                }
            }

            // a11y linting (#108)
            if config.a11y.enabled {
                let html_path = config.root.join(&config.out_dir).join("index.html");
                if html_path.exists() {
                    let html_content = std::fs::read_to_string(&html_path)?;
                    let violations = pledgepack_core::a11y::lint_html(&html_content, &config.a11y)?;
                    if !violations.is_empty() {
                        print!("{}", pledgepack_core::a11y::format_violations(&violations));
                        if config.a11y.fail_on_error
                            && violations
                                .iter()
                                .any(|v| v.severity == pledgepack_core::a11y::Severity::Error)
                        {
                            anyhow::bail!("a11y linting failed with {} error(s)", violations.iter().filter(|v| v.severity == pledgepack_core::a11y::Severity::Error).count());
                        }
                    } else {
                        println!("  \x1b[32m✓ a11y\x1b[0m no violations found");
                    }
                }
            }

            // Feature 118: Post-build optimization hooks
            {
                let out_dir_full = config.root.join(&config.out_dir);
                let html_path = out_dir_full.join("index.html");
                let chunk_names: Vec<String> = chunks.iter().map(|c| c.id.clone()).collect();
                let ctx = pledgepack_core::advanced::PostBuildContext {
                    out_dir: out_dir_full.clone(),
                    html_path: if html_path.exists() {
                        Some(html_path)
                    } else {
                        None
                    },
                    chunks: chunk_names,
                    assets: vec![],
                };
                let pb_result = pledgepack_core::advanced::run_post_build_hooks(&ctx)?;
                if pb_result.sitemap_generated {
                    println!("  \x1b[32m✓ post-build\x1b[0m sitemap.xml generated");
                }
                if pb_result.html_modified {
                    println!(
                        "  \x1b[32m✓ post-build\x1b[0m HTML meta tags injected: {} tag(s)",
                        pb_result.meta_tags_added.len()
                    );
                }
                for warning in &pb_result.warnings {
                    println!("  \x1b[33m⚠ post-build\x1b[0m {}", warning);
                }
            }

            // Feature 113: Service worker caching strategies
            if let Some(ref sw_cfg) = config.sw
                && !sw_cfg.caching.is_empty()
            {
                let runtime_rules: Vec<pledgepack_core::service_worker::RuntimeCacheRule> = sw_cfg
                    .caching
                    .iter()
                    .map(|r| pledgepack_core::service_worker::RuntimeCacheRule {
                        pattern: r.pattern.clone(),
                        strategy: match r.strategy.as_str() {
                            "cache-first" => {
                                pledgepack_core::service_worker::CacheStrategy::CacheFirst
                            }
                            "network-first" => {
                                pledgepack_core::service_worker::CacheStrategy::NetworkFirst
                            }
                            "stale-while-revalidate" => {
                                pledgepack_core::service_worker::CacheStrategy::StaleWhileRevalidate
                            }
                            "network-only" => {
                                pledgepack_core::service_worker::CacheStrategy::NetworkOnly
                            }
                            "cache-only" => {
                                pledgepack_core::service_worker::CacheStrategy::CacheOnly
                            }
                            _ => pledgepack_core::service_worker::CacheStrategy::NetworkFirst,
                        },
                    })
                    .collect();
                let sw_config = pledgepack_core::service_worker::ServiceWorkerConfig {
                    cache_name: sw_cfg.cache_name.clone(),
                    precache: vec![],
                    runtime_caching: runtime_rules,
                    offline_fallback: sw_cfg.offline_fallback.clone(),
                    skip_waiting: true,
                    clients_claim: true,
                };
                let sw_code = pledgepack_core::service_worker::generate_service_worker(&sw_config);
                let sw_path = config.root.join(&config.out_dir).join("sw.js");
                std::fs::write(&sw_path, &sw_code)?;
                println!(
                    "  \x1b[32m✓ service worker\x1b[0m {} caching rule(s) → sw.js",
                    sw_cfg.caching.len()
                );
            }

            // #81: SRI (Subresource Integrity) hashes
            // #82: CSP (Content Security Policy) generation
            if let Some(ref sec_cfg) = config.security {
                let out_dir_full = config.root.join(&config.out_dir);
                let html_path = out_dir_full.join("index.html");

                if sec_cfg.sri && html_path.exists() {
                    let html = std::fs::read_to_string(&html_path)?;
                    let html_with_sri =
                        pledgepack_core::security::inject_sri_into_html(&html, &out_dir_full);
                    std::fs::write(&html_path, &html_with_sri)?;
                    println!("  \x1b[32m✓ SRI\x1b[0m integrity hashes injected into HTML");
                }

                if sec_cfg.csp == "auto" && html_path.exists() {
                    let html = std::fs::read_to_string(&html_path)?;
                    let csp =
                        pledgepack_core::security::generate_csp_from_build(&html, &out_dir_full);
                    tracing::info!("CSP generated: {}", csp);
                    println!("  \x1b[32m✓ CSP\x1b[0m _headers file generated");
                }
            }

            if config.profile {
                println!("  \x1b[32m✓\x1b[0m Build profile:\n");
                let mut profile_table = comfy_table::Table::new();
                profile_table
                    .load_preset(comfy_table::presets::UTF8_FULL)
                    .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
                    .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
                profile_table
                    .set_header(vec!["Phase", "Time (ms)"])
                    .add_row(vec!["Parse + Transform", &result.duration_ms.to_string()])
                    .add_row(vec!["Optimize", &optimize_ms.to_string()])
                    .add_row(vec!["Emit", &emit_ms.to_string()])
                    .add_row(vec!["Dep Pre-bundle", &dep_ms.to_string()])
                    .add_row(vec!["Total", &total_ms.to_string()]);
                println!("{}\n", profile_table);
            } else {
                println!(
                    "  \x1b[32m✓\x1b[0m Built {} modules ({} cached) in {}ms\n",
                    result.modules_built, result.modules_cached, total_ms
                );
            }

            // Watch mode: monitor files and rebuild on change with debounce
            if watch {
                println!("  \x1b[90mWatching for changes... (Ctrl+C to exit)\x1b[0m\n");
                use notify::{EventKind, RecursiveMode, Watcher, recommended_watcher};
                use std::collections::HashSet;
                use std::sync::mpsc::channel;
                use std::time::{Duration, Instant};

                let (tx, rx) = channel::<notify::Result<notify::Event>>();
                let mut watcher =
                    recommended_watcher(move |res: notify::Result<notify::Event>| {
                        let _ = tx.send(res);
                    })?;

                watcher.watch(&config.root, RecursiveMode::Recursive)?;

                let debounce_ms = Duration::from_millis(200);
                let mut last_rebuild: Option<Instant> = None;
                let mut pending_paths: HashSet<std::path::PathBuf> = HashSet::new();

                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(Ok(event)) => {
                            let is_relevant = matches!(
                                event.kind,
                                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                            );
                            if !is_relevant {
                                continue;
                            }

                            for path in &event.paths {
                                let path_str = path.to_string_lossy();
                                if path_str.contains("node_modules")
                                    || path_str.contains(".pledge")
                                    || path_str.contains("target")
                                    || path_str.contains(".git")
                                {
                                    continue;
                                }
                                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                                if !matches!(
                                    ext,
                                    "ts" | "tsx"
                                        | "js"
                                        | "jsx"
                                        | "css"
                                        | "scss"
                                        | "less"
                                        | "vue"
                                        | "svelte"
                                        | "html"
                                        | "json"
                                ) {
                                    continue;
                                }
                                pending_paths.insert(path.clone());
                            }

                            last_rebuild = Some(Instant::now());
                        }
                        Ok(Err(e)) => {
                            tracing::warn!("Watch error: {}", e);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if let Some(last) = last_rebuild
                                && last.elapsed() >= debounce_ms
                                && !pending_paths.is_empty()
                            {
                                let changed: Vec<_> = pending_paths.drain().collect();
                                let changed_count = changed.len();

                                for path in &changed {
                                    let rel = pledgepack_core::normalize_path(
                                        path.strip_prefix(&config.root).unwrap_or(path),
                                    );
                                    println!("  \x1b[33mchanged:\x1b[0m {}", rel);
                                }

                                println!("  \x1b[36mrebuilding...\x1b[0m");
                                let rebuild_start = std::time::Instant::now();

                                match engine.build().await {
                                    Ok(_result) => {
                                        let elapsed = rebuild_start.elapsed();
                                        println!(
                                            "  \x1b[32m✓ rebuilt\x1b[0m {} file(s) in {:.0?}\n",
                                            changed_count, elapsed
                                        );
                                    }
                                    Err(e) => {
                                        eprintln!("  \x1b[31m✗ rebuild failed:\x1b[0m {}\n", e);
                                    }
                                }

                                last_rebuild = None;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            println!("  \x1b[33mWatch channel disconnected, exiting...\x1b[0m");
                            break;
                        }
                    }
                }
            }
        }

        Commands::Preview { port } => {
            let port = port.unwrap_or(4000);
            let out_dir = config.out_dir.clone();

            if !out_dir.exists() {
                anyhow::bail!(
                    "Output directory '{}' not found. Run `pledge build` first.",
                    out_dir.display()
                );
            }

            println!(
                "\n  \x1b[36mpledge preview\x1b[0m — serving {} on http://localhost:{}\n",
                out_dir.display(),
                port
            );

            // SPA fallback: serve index.html for any route that doesn't match a static file
            let index_path = out_dir.join("index.html");
            let serve_dir = tower_http::services::ServeDir::new(&out_dir)
                .fallback(tower_http::services::ServeFile::new(index_path));
            let app = axum::Router::new().fallback_service(serve_dir);

            let addr = format!("127.0.0.1:{}", port);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }

        Commands::Create {
            template,
            name,
            flash,
        } => {
            use console::style;

            // CSS framework and package manager choices from wizard
            let mut css_choice: Option<String> = None;
            let mut pm_choice: Option<String> = None;

            // #12: Flash create — skip wizard, use defaults, minimal output
            let (template, project_name) = if flash {
                let project_name = name.unwrap_or_else(|| "my-app".to_string());
                let template = template.unwrap_or_else(|| "pledge".to_string());
                (template, project_name)
            } else if template.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                use dialoguer::{Confirm, Input, Select};

                println!(
                    "\n  {} Pledgepack Create Wizard\n",
                    style("pledge create").cyan().bold()
                );

                // Project name
                let default_name = name.clone().unwrap_or_else(|| "my-app".to_string());
                let project_name: String = Input::new()
                    .with_prompt("Project name")
                    .default(default_name)
                    .interact_text()?;

                // Framework selection
                let frameworks = [
                    "PledgeStack (React + Rust full-stack)",
                    "React",
                    "Vue",
                    "Svelte",
                    "Solid",
                    "Next.js (React adapter)",
                    "TanStack Router (React adapter)",
                    "Vanilla (no framework)",
                ];
                let framework_idx = Select::new()
                    .with_prompt("Select framework")
                    .items(&frameworks)
                    .default(0)
                    .interact()?;

                let template = match framework_idx {
                    0 => "pledge",
                    1 => "react",
                    2 => "vue",
                    3 => "svelte",
                    4 => "solid",
                    5 => "next",
                    6 => "tanstack",
                    _ => "vanilla",
                }
                .to_string();

                // TypeScript toggle
                let typescript = Confirm::new()
                    .with_prompt("Use TypeScript?")
                    .default(true)
                    .interact()?;

                // CSS framework
                let css_frameworks = [
                    "None (plain CSS)",
                    "Tailwind CSS",
                    "UnoCSS",
                    "Panda CSS",
                    "Vanilla Extract",
                ];
                let css_idx = Select::new()
                    .with_prompt("CSS framework")
                    .items(&css_frameworks)
                    .default(0)
                    .interact()?;

                let css = match css_idx {
                    1 => "tailwind",
                    2 => "unocss",
                    3 => "panda-css",
                    4 => "vanilla-extract",
                    _ => "none",
                };
                css_choice = Some(css.to_string());

                // Package manager
                let pkg_managers = ["npm", "yarn", "pnpm", "bun"];
                let pm_idx = Select::new()
                    .with_prompt("Package manager")
                    .items(&pkg_managers)
                    .default(0)
                    .interact()?;

                let pm = pkg_managers[pm_idx];
                pm_choice = Some(pm.to_string());

                // Summary
                println!(
                    "\n  {} Project will be created with:",
                    style("Summary").dim()
                );
                println!("    Name:       {}", project_name);
                println!("    Framework:  {}", template);
                println!("    TypeScript: {}", if typescript { "yes" } else { "no" });
                println!();

                let proceed = Confirm::new()
                    .with_prompt("Proceed?")
                    .default(true)
                    .interact()?;

                if !proceed {
                    println!("\n  {}\n", style("Cancelled.").dim());
                    return Ok(());
                }

                (template, project_name)
            } else {
                let project_name = name.unwrap_or_else(|| "my-app".to_string());
                let template = template.unwrap_or_else(|| {
                    // Default to pledge, but auto-detect if existing project files suggest otherwise
                    let detection = detect::detect_project(std::path::Path::new("."));
                    if detection.framework == detect::DetectedFramework::Vanilla {
                        "pledge".to_string()
                    } else {
                        detection.framework.as_str().to_string()
                    }
                });
                (template, project_name)
            };

            let project_dir = std::path::Path::new(&project_name);

            if project_dir.exists() {
                println!(
                    "\n  \x1b[31mError\x1b[0m Directory '{}' already exists\n",
                    project_name
                );
                return Ok(());
            }

            // #5: Cached create templates — check if template is cached locally
            let cache_dir = dirs::cache_dir()
                .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
                .map(|d| d.join(".pledge").join("templates").join(&template));
            let cached = cache_dir
                .as_ref()
                .map(|d| d.join("package.json").exists())
                .unwrap_or(false);

            if cached {
                // Copy from cache — instant template reuse
                std::fs::create_dir_all(project_dir)?;
                std::fs::create_dir_all(project_dir.join("src"))?;
                if let Some(ref cache_dir) = cache_dir {
                    copy_dir_recursive(cache_dir, project_dir)?;
                }

                // Update package.json with actual project name
                let pkg_path = project_dir.join("package.json");
                let mut pkg: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(&pkg_path)?)?;
                if let Some(obj) = pkg.as_object_mut() {
                    obj.insert(
                        "name".to_string(),
                        serde_json::Value::String(project_name.clone()),
                    );
                }
                std::fs::write(pkg_path, serde_json::to_string_pretty(&pkg)?)?;

                // Regenerate index.html with actual project name
                std::fs::write(
                    project_dir.join("index.html"),
                    html::generate_default_html("src/index.tsx", &project_name),
                )?;

                // Update entry file project name for vue/svelte/solid templates
                let entry_path = project_dir.join("src/index.tsx");
                let entry_content = std::fs::read_to_string(&entry_path).unwrap_or_default();
                // Replace any previous project name placeholder with the new one
                let updated_entry = entry_content.replace("__PLEDGE_PROJECT_NAME__", &project_name);
                std::fs::write(entry_path, updated_entry)?;

                // Also replace placeholder in app/page.tsx for PledgeStack
                let page_path = project_dir.join("app/page.tsx");
                if page_path.exists() {
                    let page_content = std::fs::read_to_string(&page_path).unwrap_or_default();
                    let updated_page =
                        page_content.replace("__PLEDGE_PROJECT_NAME__", &project_name);
                    std::fs::write(page_path, updated_page)?;
                }

                // Re-inject framework dependencies (cache may have stale package.json)
                let deps = framework_deps(&template);
                let pkg_path = project_dir.join("package.json");
                let mut pkg: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(&pkg_path).unwrap_or_else(|_| "{}".to_string()),
                )?;
                if let Some(obj) = pkg.as_object_mut() {
                    obj.insert("dependencies".to_string(), deps);
                }
                std::fs::write(pkg_path, serde_json::to_string_pretty(&pkg)?)?;
            } else {
                // Generate from scratch and cache
                std::fs::create_dir_all(project_dir)?;
                std::fs::create_dir_all(project_dir.join("src"))?;

                // Create package.json with framework dependencies and optional CSS framework devDependencies
                let mut pkg = serde_json::json!({
                    "name": project_name,
                    "version": "0.1.0",
                    "scripts": {
                        "dev": "pledge dev",
                        "build": "pledge build",
                        "preview": "pledge preview"
                    }
                });

                // Add framework dependencies based on template
                let deps = framework_deps(&template);
                if let Some(obj) = pkg.as_object_mut() {
                    obj.insert("dependencies".to_string(), deps);
                }

                if let Some(ref css) = css_choice {
                    let dev_deps = css_framework_dev_deps(css);
                    if let Some(obj) = pkg.as_object_mut() {
                        obj.insert("devDependencies".to_string(), dev_deps);
                    }
                }
                std::fs::write(
                    project_dir.join("package.json"),
                    serde_json::to_string_pretty(&pkg)?,
                )?;

                // Create pledge.config.ts
                let pledge_config = match template.as_str() {
                    "pledge" => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'pledge',
  devServer: {
    port: 3000,
    hmr: true,
  },
});
"#
                    }
                    "vue" => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'vue',
  devServer: {
    port: 3000,
    hmr: true,
  },
});
"#
                    }
                    "svelte" => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'svelte',
  devServer: {
    port: 3000,
    hmr: true,
  },
});
"#
                    }
                    "solid" => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'solid',
  devServer: {
    port: 3000,
    hmr: true,
  },
});
"#
                    }
                    "next" => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'next',
  devServer: {
    port: 3000,
    hmr: true,
  },
  plugins: [],
});
"#
                    }
                    "tanstack" => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'tanstack',
  devServer: {
    port: 3000,
    hmr: true,
  },
});
"#
                    }
                    _ => {
                        r#"import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['src/index.tsx'],
  framework: 'react',
  devServer: {
    port: 3000,
    hmr: true,
  },
});
"#
                    }
                };
                std::fs::write(project_dir.join("pledge.config.ts"), pledge_config)?;

                // Create .env file
                std::fs::write(
                    project_dir.join(".env"),
                    r#"# Pledge environment variables
PLEDGE_APP_NAME=My App
PLEDGE_API_URL=http://localhost:8080
"#,
                )?;

                // Create .env.local (gitignored)
                std::fs::write(
                    project_dir.join(".env.local"),
                    r#"# Local environment variables (not committed)
PLEDGE_API_URL=http://localhost:3000
"#,
                )?;

                // Create index.html
                std::fs::write(
                    project_dir.join("index.html"),
                    html::generate_default_html("src/index.tsx", &project_name),
                )?;

                // Create entry file based on template
                // Use __PLEDGE_PROJECT_NAME__ placeholder for cache-friendly templates
                let entry = match template.as_str() {
                    "pledge" => {
                        r##"// PledgeStack template — React frontend + Rust backend
import { createRoot } from 'react-dom/client';

function App() {
  return (
    <div style={{
      display: "flex",
      flexDirection: "column",
      alignItems: "center",
      justifyContent: "center",
      minHeight: "100vh",
      margin: 0,
      fontFamily: "system-ui, -apple-system, sans-serif",
      background: "#000",
      color: "#e0e0e0",
      overflow: "hidden",
      position: "relative",
    }}>
      <div style={{
        position: "absolute",
        top: "50%",
        left: "50%",
        transform: "translate(-50%, -50%)",
        width: "600px",
        height: "600px",
        borderRadius: "50%",
        background: "radial-gradient(circle, rgba(99,102,241,0.15) 0%, transparent 70%)",
        filter: "blur(60px)",
        pointerEvents: "none",
      }} />
      <div style={{ position: "relative", zIndex: 1, textAlign: "center" }}>
        <h1 style={{
          fontSize: "clamp(3rem, 12vw, 7rem)",
          fontWeight: 900,
          letterSpacing: "-0.05em",
          margin: 0,
          background: "linear-gradient(135deg, #6366f1 0%, #a855f7 50%, #ec4899 100%)",
          WebkitBackgroundClip: "text",
          WebkitTextFillColor: "transparent",
          backgroundClip: "text",
          lineHeight: 1,
        }}>PledgePack</h1>
        <div style={{
          display: "inline-block",
          marginTop: "1rem",
          padding: "0.3rem 1rem",
          borderRadius: "9999px",
          background: "rgba(99,102,241,0.15)",
          border: "1px solid rgba(99,102,241,0.3)",
          fontSize: "0.9rem",
          fontWeight: 600,
          color: "#a5b4fc",
          letterSpacing: "0.05em",
        }}>PledgeStack</div>
        <p style={{
          fontSize: "clamp(0.9rem, 2vw, 1.1rem)",
          color: "#666",
          marginTop: "1.5rem",
          maxWidth: "480px",
          lineHeight: 1.6,
        }}>Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, file-based routing — React frontend with a Rust backend.</p>
        <div style={{
          marginTop: "2rem",
          display: "flex",
          gap: "0.5rem",
          flexWrap: "wrap",
          justifyContent: "center",
        }}>
          {["Rust Engine", "Oxc Parser", "Instant HMR", "File Routing"].map((tag) => (
            <span key={tag} style={{
              padding: "0.25rem 0.75rem",
              borderRadius: "6px",
              background: "rgba(255,255,255,0.05)",
              border: "1px solid rgba(255,255,255,0.08)",
              fontSize: "0.75rem",
              color: "#888",
            }}>{tag}</span>
          ))}
        </div>
      </div>
    </div>
  );
}

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(<App />);
}
export default App;
"##
                    }
                    "vue" => {
                        r##"// Vue template
const root = document.getElementById("root");
if (root) {
  root.innerHTML = `
    <div style="display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh;margin:0;font-family:system-ui,-apple-system,sans-serif;background:#000;color:#e0e0e0;overflow:hidden;position:relative;">
      <div style="position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);width:600px;height:600px;border-radius:50%;background:radial-gradient(circle,rgba(66,184,131,0.15) 0%,transparent 70%);filter:blur(60px);pointer-events:none;"></div>
      <div style="position:relative;z-index:1;text-align:center;">
        <h1 style="font-size:clamp(3rem,12vw,7rem);font-weight:900;letter-spacing:-0.05em;margin:0;background:linear-gradient(135deg,#42b883 0%,#35495e 100%);-webkit-background-clip:text;-webkit-text-fill-color:transparent;background-clip:text;line-height:1;">PledgePack</h1>
        <div style="display:inline-block;margin-top:1rem;padding:0.3rem 1rem;border-radius:9999px;background:rgba(66,184,131,0.15);border:1px solid rgba(66,184,131,0.3);font-size:0.9rem;font-weight:600;color:#42b883;letter-spacing:0.05em;">Vue</div>
        <p style="font-size:clamp(0.9rem,2vw,1.1rem);color:#666;margin-top:1.5rem;max-width:480px;line-height:1.6;">Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — built for Vue.</p>
        <div style="margin-top:2rem;display:flex;gap:0.5rem;flex-wrap:wrap;justify-content:center;">
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Rust Engine</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Oxc Parser</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Instant HMR</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Zero Config</span>
        </div>
      </div>
    </div>
  `;
}
export default {};
"##
                    }
                    "svelte" => {
                        r##"// Svelte template
const root = document.getElementById("root");
if (root) {
  root.innerHTML = `
    <div style="display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh;margin:0;font-family:system-ui,-apple-system,sans-serif;background:#000;color:#e0e0e0;overflow:hidden;position:relative;">
      <div style="position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);width:600px;height:600px;border-radius:50%;background:radial-gradient(circle,rgba(255,62,0,0.15) 0%,transparent 70%);filter:blur(60px);pointer-events:none;"></div>
      <div style="position:relative;z-index:1;text-align:center;">
        <h1 style="font-size:clamp(3rem,12vw,7rem);font-weight:900;letter-spacing:-0.05em;margin:0;background:linear-gradient(135deg,#ff3e00 0%,#ff8e00 100%);-webkit-background-clip:text;-webkit-text-fill-color:transparent;background-clip:text;line-height:1;">PledgePack</h1>
        <div style="display:inline-block;margin-top:1rem;padding:0.3rem 1rem;border-radius:9999px;background:rgba(255,62,0,0.15);border:1px solid rgba(255,62,0,0.3);font-size:0.9rem;font-weight:600;color:#ff8e00;letter-spacing:0.05em;">Svelte</div>
        <p style="font-size:clamp(0.9rem,2vw,1.1rem);color:#666;margin-top:1.5rem;max-width:480px;line-height:1.6;">Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — built for Svelte.</p>
        <div style="margin-top:2rem;display:flex;gap:0.5rem;flex-wrap:wrap;justify-content:center;">
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Rust Engine</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Oxc Parser</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Instant HMR</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Zero Config</span>
        </div>
      </div>
    </div>
  `;
}
export default {};
"##
                    }
                    "solid" => {
                        r##"// Solid template
const root = document.getElementById("root");
if (root) {
  root.innerHTML = `
    <div style="display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh;margin:0;font-family:system-ui,-apple-system,sans-serif;background:#000;color:#e0e0e0;overflow:hidden;position:relative;">
      <div style="position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);width:600px;height:600px;border-radius:50%;background:radial-gradient(circle,rgba(44,79,124,0.15) 0%,transparent 70%);filter:blur(60px);pointer-events:none;"></div>
      <div style="position:relative;z-index:1;text-align:center;">
        <h1 style="font-size:clamp(3rem,12vw,7rem);font-weight:900;letter-spacing:-0.05em;margin:0;background:linear-gradient(135deg,#2c4f7c 0%,#61dafb 100%);-webkit-background-clip:text;-webkit-text-fill-color:transparent;background-clip:text;line-height:1;">PledgePack</h1>
        <div style="display:inline-block;margin-top:1rem;padding:0.3rem 1rem;border-radius:9999px;background:rgba(44,79,124,0.15);border:1px solid rgba(97,218,251,0.3);font-size:0.9rem;font-weight:600;color:#61dafb;letter-spacing:0.05em;">Solid</div>
        <p style="font-size:clamp(0.9rem,2vw,1.1rem);color:#666;margin-top:1.5rem;max-width:480px;line-height:1.6;">Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — built for Solid.</p>
        <div style="margin-top:2rem;display:flex;gap:0.5rem;flex-wrap:wrap;justify-content:center;">
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Rust Engine</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Oxc Parser</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Instant HMR</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Zero Config</span>
        </div>
      </div>
    </div>
  `;
}
export default {};
"##
                    }
                    "vanilla" => {
                        r#"// Vanilla template
const root = document.getElementById("root");
if (root) {
  root.innerHTML = `
    <div style="display:flex;flex-direction:column;align-items:center;justify-content:center;min-height:100vh;margin:0;font-family:system-ui,-apple-system,sans-serif;background:#000;color:#e0e0e0;overflow:hidden;position:relative;">
      <div style="position:absolute;top:50%;left:50%;transform:translate(-50%,-50%);width:600px;height:600px;border-radius:50%;background:radial-gradient(circle,rgba(251,191,36,0.12) 0%,transparent 70%);filter:blur(60px);pointer-events:none;"></div>
      <div style="position:relative;z-index:1;text-align:center;">
        <h1 style="font-size:clamp(3rem,12vw,7rem);font-weight:900;letter-spacing:-0.05em;margin:0;color:#fbbf24;line-height:1;">PledgePack</h1>
        <div style="display:inline-block;margin-top:1rem;padding:0.3rem 1rem;border-radius:9999px;background:rgba(251,191,36,0.12);border:1px solid rgba(251,191,36,0.3);font-size:0.9rem;font-weight:600;color:#fbbf24;letter-spacing:0.05em;">Vanilla</div>
        <p style="font-size:clamp(0.9rem,2vw,1.1rem);color:#666;margin-top:1.5rem;max-width:480px;line-height:1.6;">Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — no framework required.</p>
        <div style="margin-top:2rem;display:flex;gap:0.5rem;flex-wrap:wrap;justify-content:center;">
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Rust Engine</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Oxc Parser</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Instant HMR</span>
          <span style="padding:0.25rem 0.75rem;border-radius:6px;background:rgba(255,255,255,0.05);border:1px solid rgba(255,255,255,0.08);font-size:0.75rem;color:#888;">Zero Config</span>
        </div>
      </div>
    </div>
  `;
}
export default {};
"#
                    }
                    "next" => {
                        r##"// Next.js template
import { createRoot } from 'react-dom/client';

function App() {
  return (
    <div style={{
      display: "flex",
      flexDirection: "column",
      alignItems: "center",
      justifyContent: "center",
      minHeight: "100vh",
      margin: 0,
      fontFamily: "system-ui, -apple-system, sans-serif",
      background: "#000",
      color: "#e0e0e0",
      overflow: "hidden",
      position: "relative",
    }}>
      <div style={{
        position: "absolute",
        top: "50%",
        left: "50%",
        transform: "translate(-50%, -50%)",
        width: "600px",
        height: "600px",
        borderRadius: "50%",
        background: "radial-gradient(circle, rgba(255,255,255,0.08) 0%, transparent 70%)",
        filter: "blur(60px)",
        pointerEvents: "none",
      }} />
      <div style={{ position: "relative", zIndex: 1, textAlign: "center" }}>
        <h1 style={{
          fontSize: "clamp(3rem, 12vw, 7rem)",
          fontWeight: 900,
          letterSpacing: "-0.05em",
          margin: 0,
          background: "linear-gradient(135deg, #ffffff 0%, #888888 100%)",
          WebkitBackgroundClip: "text",
          WebkitTextFillColor: "transparent",
          backgroundClip: "text",
          lineHeight: 1,
        }}>PledgePack</h1>
        <div style={{
          display: "inline-block",
          marginTop: "1rem",
          padding: "0.3rem 1rem",
          borderRadius: "9999px",
          background: "rgba(255,255,255,0.08)",
          border: "1px solid rgba(255,255,255,0.15)",
          fontSize: "0.9rem",
          fontWeight: 600,
          color: "#ccc",
          letterSpacing: "0.05em",
        }}>Next.js</div>
        <p style={{
          fontSize: "clamp(0.9rem, 2vw, 1.1rem)",
          color: "#666",
          marginTop: "1.5rem",
          maxWidth: "480px",
          lineHeight: 1.6,
        }}>Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — built for Next.js.</p>
        <div style={{
          marginTop: "2rem",
          display: "flex",
          gap: "0.5rem",
          flexWrap: "wrap",
          justifyContent: "center",
        }}>
          {["Rust Engine", "Oxc Parser", "Instant HMR", "Zero Config"].map((tag) => (
            <span key={tag} style={{
              padding: "0.25rem 0.75rem",
              borderRadius: "6px",
              background: "rgba(255,255,255,0.05)",
              border: "1px solid rgba(255,255,255,0.08)",
              fontSize: "0.75rem",
              color: "#888",
            }}>{tag}</span>
          ))}
        </div>
      </div>
    </div>
  );
}

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(<App />);
}
export default App;
"##
                    }
                    "tanstack" => {
                        r##"// TanStack template
import { createRoot } from 'react-dom/client';

function App() {
  return (
    <div style={{
      display: "flex",
      flexDirection: "column",
      alignItems: "center",
      justifyContent: "center",
      minHeight: "100vh",
      margin: 0,
      fontFamily: "system-ui, -apple-system, sans-serif",
      background: "#000",
      color: "#e0e0e0",
      overflow: "hidden",
      position: "relative",
    }}>
      <div style={{
        position: "absolute",
        top: "50%",
        left: "50%",
        transform: "translate(-50%, -50%)",
        width: "600px",
        height: "600px",
        borderRadius: "50%",
        background: "radial-gradient(circle, rgba(0,180,216,0.15) 0%, transparent 70%)",
        filter: "blur(60px)",
        pointerEvents: "none",
      }} />
      <div style={{ position: "relative", zIndex: 1, textAlign: "center" }}>
        <h1 style={{
          fontSize: "clamp(3rem, 12vw, 7rem)",
          fontWeight: 900,
          letterSpacing: "-0.05em",
          margin: 0,
          background: "linear-gradient(135deg, #00b4d8 0%, #0077b6 100%)",
          WebkitBackgroundClip: "text",
          WebkitTextFillColor: "transparent",
          backgroundClip: "text",
          lineHeight: 1,
        }}>PledgePack</h1>
        <div style={{
          display: "inline-block",
          marginTop: "1rem",
          padding: "0.3rem 1rem",
          borderRadius: "9999px",
          background: "rgba(0,180,216,0.15)",
          border: "1px solid rgba(0,180,216,0.3)",
          fontSize: "0.9rem",
          fontWeight: 600,
          color: "#00b4d8",
          letterSpacing: "0.05em",
        }}>TanStack</div>
        <p style={{
          fontSize: "clamp(0.9rem, 2vw, 1.1rem)",
          color: "#666",
          marginTop: "1.5rem",
          maxWidth: "480px",
          lineHeight: 1.6,
        }}>Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — built for TanStack.</p>
        <div style={{
          marginTop: "2rem",
          display: "flex",
          gap: "0.5rem",
          flexWrap: "wrap",
          justifyContent: "center",
        }}>
          {["Rust Engine", "Oxc Parser", "Instant HMR", "Zero Config"].map((tag) => (
            <span key={tag} style={{
              padding: "0.25rem 0.75rem",
              borderRadius: "6px",
              background: "rgba(255,255,255,0.05)",
              border: "1px solid rgba(255,255,255,0.08)",
              fontSize: "0.75rem",
              color: "#888",
            }}>{tag}</span>
          ))}
        </div>
      </div>
    </div>
  );
}

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(<App />);
}
export default App;
"##
                    }
                    _ => {
                        r##"// React template
import { createRoot } from 'react-dom/client';

function App() {
  return (
    <div style={{
      display: "flex",
      flexDirection: "column",
      alignItems: "center",
      justifyContent: "center",
      minHeight: "100vh",
      margin: 0,
      fontFamily: "system-ui, -apple-system, sans-serif",
      background: "#000",
      color: "#e0e0e0",
      overflow: "hidden",
      position: "relative",
    }}>
      <div style={{
        position: "absolute",
        top: "50%",
        left: "50%",
        transform: "translate(-50%, -50%)",
        width: "600px",
        height: "600px",
        borderRadius: "50%",
        background: "radial-gradient(circle, rgba(99,102,241,0.15) 0%, transparent 70%)",
        filter: "blur(60px)",
        pointerEvents: "none",
      }} />
      <div style={{ position: "relative", zIndex: 1, textAlign: "center" }}>
        <h1 style={{
          fontSize: "clamp(3rem, 12vw, 7rem)",
          fontWeight: 900,
          letterSpacing: "-0.05em",
          margin: 0,
          background: "linear-gradient(135deg, #6366f1 0%, #a855f7 50%, #ec4899 100%)",
          WebkitBackgroundClip: "text",
          WebkitTextFillColor: "transparent",
          backgroundClip: "text",
          lineHeight: 1,
        }}>PledgePack</h1>
        <div style={{
          display: "inline-block",
          marginTop: "1rem",
          padding: "0.3rem 1rem",
          borderRadius: "9999px",
          background: "rgba(99,102,241,0.15)",
          border: "1px solid rgba(99,102,241,0.3)",
          fontSize: "0.9rem",
          fontWeight: 600,
          color: "#a5b4fc",
          letterSpacing: "0.05em",
        }}>React</div>
        <p style={{
          fontSize: "clamp(0.9rem, 2vw, 1.1rem)",
          color: "#666",
          marginTop: "1.5rem",
          maxWidth: "480px",
          lineHeight: 1.6,
        }}>Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, and on-demand transforms — built for React.</p>
        <div style={{
          marginTop: "2rem",
          display: "flex",
          gap: "0.5rem",
          flexWrap: "wrap",
          justifyContent: "center",
        }}>
          {["Rust Engine", "Oxc Parser", "Instant HMR", "Zero Config"].map((tag) => (
            <span key={tag} style={{
              padding: "0.25rem 0.75rem",
              borderRadius: "6px",
              background: "rgba(255,255,255,0.05)",
              border: "1px solid rgba(255,255,255,0.08)",
              fontSize: "0.75rem",
              color: "#888",
            }}>{tag}</span>
          ))}
        </div>
      </div>
    </div>
  );
}

const root = document.getElementById("root");
if (root) {
  createRoot(root).render(<App />);
}
export default App;
"##
                    }
                };

                let entry_content = entry.replace("__PLEDGE_PROJECT_NAME__", &project_name);
                std::fs::write(project_dir.join("src/index.tsx"), entry_content)?;

                // PledgeStack app/page.tsx content (declared here so it's accessible in cache block)
                let page_content: Option<&str> = if template == "pledge" {
                    Some(
                        r##"export default function Home() {
  return (
    <div style={{
      display: "flex",
      flexDirection: "column",
      alignItems: "center",
      justifyContent: "center",
      minHeight: "100vh",
      margin: 0,
      fontFamily: "system-ui, -apple-system, sans-serif",
      background: "#000",
      color: "#e0e0e0",
      overflow: "hidden",
      position: "relative",
    }}>
      <div style={{
        position: "absolute",
        top: "50%",
        left: "50%",
        transform: "translate(-50%, -50%)",
        width: "600px",
        height: "600px",
        borderRadius: "50%",
        background: "radial-gradient(circle, rgba(99,102,241,0.15) 0%, transparent 70%)",
        filter: "blur(60px)",
        pointerEvents: "none",
      }} />
      <div style={{ position: "relative", zIndex: 1, textAlign: "center" }}>
        <h1 style={{
          fontSize: "clamp(3rem, 12vw, 7rem)",
          fontWeight: 900,
          letterSpacing: "-0.05em",
          margin: 0,
          background: "linear-gradient(135deg, #6366f1 0%, #a855f7 50%, #ec4899 100%)",
          WebkitBackgroundClip: "text",
          WebkitTextFillColor: "transparent",
          backgroundClip: "text",
          lineHeight: 1,
        }}>PledgePack</h1>
        <div style={{
          display: "inline-block",
          marginTop: "1rem",
          padding: "0.3rem 1rem",
          borderRadius: "9999px",
          background: "rgba(99,102,241,0.15)",
          border: "1px solid rgba(99,102,241,0.3)",
          fontSize: "0.9rem",
          fontWeight: 600,
          color: "#a5b4fc",
          letterSpacing: "0.05em",
        }}>PledgeStack</div>
        <p style={{
          fontSize: "clamp(0.9rem, 2vw, 1.1rem)",
          color: "#666",
          marginTop: "1.5rem",
          maxWidth: "480px",
          lineHeight: 1.6,
        }}>Native-speed bundler powered by Rust + Oxc. Zero config, instant HMR, file-based routing — React frontend with a Rust backend.</p>
        <div style={{
          marginTop: "2rem",
          display: "flex",
          gap: "0.5rem",
          flexWrap: "wrap",
          justifyContent: "center",
        }}>
          {["Rust Engine", "Oxc Parser", "Instant HMR", "File Routing"].map((tag) => (
            <span key={tag} style={{
              padding: "0.25rem 0.75rem",
              borderRadius: "6px",
              background: "rgba(255,255,255,0.05)",
              border: "1px solid rgba(255,255,255,0.08)",
              fontSize: "0.75rem",
              color: "#888",
            }}>{tag}</span>
          ))}
        </div>
      </div>
    </div>
  );
}
"##,
                    )
                } else {
                    None
                };

                // Create PledgeStack app directory structure (Next.js-style file-based routing)
                if template == "pledge" {
                    std::fs::create_dir_all(project_dir.join("app"))?;
                    // Root layout
                    std::fs::write(
                        project_dir.join("app/layout.tsx"),
                        r#"export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
"#,
                    )?;
                    // Home page
                    if let Some(page) = page_content {
                        std::fs::write(
                            project_dir.join("app/page.tsx"),
                            page.replace("__PLEDGE_PROJECT_NAME__", &project_name),
                        )?;
                    }
                    // API route example
                    std::fs::create_dir_all(project_dir.join("app/api/hello"))?;
                    std::fs::write(
                        project_dir.join("app/api/hello/route.ts"),
                        r#"export async function GET() {
  return Response.json({ message: "Hello from PledgeStack API!" });
}
"#,
                    )?;
                    // Server entry point (Rust backend)
                    std::fs::create_dir_all(project_dir.join("server"))?;
                    std::fs::write(
                        project_dir.join("server/main.rs"),
                        r#"// PledgeStack server entry point
// This file is compiled with cargo and runs the Rust backend

fn main() {
    println!("PledgeStack server starting...");
    // Add your server logic here
}
"#,
                    )?;
                    // Cargo.toml for the server
                    std::fs::write(
                        project_dir.join("server/Cargo.toml"),
                        r#"[package]
name = "pledgestack-server"
version = "0.1.0"
edition = "2021"

[dependencies]
"#,
                    )?;
                    // Update .gitignore for PledgeStack
                    std::fs::write(
                        project_dir.join(".gitignore"),
                        ".pledge/\ntarget/\nnode_modules/\n.env.local\npledge-env.d.ts\nserver/target/\n",
                    )?;
                }

                // Create utils.ts
                std::fs::write(
                    project_dir.join("src/utils.ts"),
                    r#"export function greet(name: string): string {
  return `Hello, ${name}!`;
}
"#,
                )?;

                // Create .gitignore (skip if already created for pledge)
                if template != "pledge" {
                    std::fs::write(
                        project_dir.join(".gitignore"),
                        ".pledge/\ntarget/\nnode_modules/\n.env.local\npledge-env.d.ts\n",
                    )?;
                }

                // Create CSS framework config files based on wizard selection
                if let Some(ref css) = css_choice {
                    match css.as_str() {
                        "tailwind" => {
                            std::fs::write(
                                project_dir.join("tailwind.config.js"),
                                r#"/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx,js,jsx}'],
  theme: { extend: {} },
  plugins: [],
};
"#,
                            )?;
                            std::fs::write(
                                project_dir.join("postcss.config.js"),
                                r#"export default {
  plugins: {
    tailwindcss: {},
    autoprefixer: {},
  },
};
"#,
                            )?;
                        }
                        "unocss" => {
                            std::fs::write(
                                project_dir.join("uno.config.ts"),
                                r#"import { defineConfig } from 'unocss';

export default defineConfig({
  presets: [],
});
"#,
                            )?;
                        }
                        _ => {}
                    }
                }

                // Cache template for instant reuse (#5)
                if let Some(ref cache_dir) = cache_dir {
                    let _ = std::fs::create_dir_all(cache_dir);
                    let _ = std::fs::create_dir_all(cache_dir.join("src"));
                    let _ = copy_dir_recursive(project_dir, cache_dir);
                    // Write placeholder version of entry file to cache
                    let _ = std::fs::write(cache_dir.join("src/index.tsx"), entry);
                    // Write placeholder version of app/page.tsx to cache (PledgeStack)
                    if let Some(pc) = page_content {
                        let _ = std::fs::write(cache_dir.join("app/page.tsx"), pc);
                    }
                }
            }

            // #8: Pre-warmed module graph — scan source files on create
            let pledge_dir = project_dir.join(".pledge");
            let _ = std::fs::create_dir_all(&pledge_dir);
            prewarm_module_graph(project_dir, &pledge_dir);

            // Auto-install dependencies if package.json has them
            let pkg_json_path = project_dir.join("package.json");
            let has_deps = std::fs::read_to_string(&pkg_json_path)
                .map(|c| c.contains("\"dependencies\""))
                .unwrap_or(false);
            if has_deps {
                // On Windows, npm/yarn/pnpm are .cmd files — must use cmd /c
                let is_windows = cfg!(target_os = "windows");

                let pm_check = |bin: &str| -> bool {
                    if is_windows {
                        std::process::Command::new("cmd")
                            .args(["/c", &format!("{} --version", bin)])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false)
                    } else {
                        std::process::Command::new(bin)
                            .arg("--version")
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false)
                    }
                };

                let pm = if let Some(ref pm) = pm_choice {
                    pm.clone()
                } else if pm_check("pnpm") {
                    "pnpm".to_string()
                } else if pm_check("yarn") {
                    "yarn".to_string()
                } else {
                    "npm".to_string()
                };

                let install_cmd = match pm.as_str() {
                    "yarn" => "yarn",
                    "pnpm" => "pnpm install",
                    "bun" => "bun install",
                    _ => "npm install",
                };

                print!("  \x1b[90mInstalling dependencies with {}...\x1b[0m", pm);
                let install_result = if is_windows {
                    std::process::Command::new("cmd")
                        .args(["/c", install_cmd])
                        .current_dir(project_dir)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                } else {
                    let parts: Vec<&str> = install_cmd.split_whitespace().collect();
                    std::process::Command::new(parts[0])
                        .args(&parts[1..])
                        .current_dir(project_dir)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                };

                match install_result {
                    Ok(s) if s.success() => {
                        println!("\r  \x1b[32m✓\x1b[0m Dependencies installed ({})    ", pm);
                    }
                    _ => {
                        eprintln!(
                            "\r  \x1b[33m⚠\x1b[0m Auto-install failed — run `{}` manually    ",
                            install_cmd
                        );
                    }
                }
            }

            if flash {
                println!("\n  \x1b[32m✓\x1b[0m {} — {}\n", template, project_name);
                println!("  \x1b[90mcd {} && pledge dev\x1b[0m\n", project_name);
            } else {
                println!(
                    "\n  \x1b[32m✓\x1b[0m Created {} project: {}\n",
                    template, project_name
                );
                if let Some(ref pm) = pm_choice {
                    let install_cmd = match pm.as_str() {
                        "yarn" => "yarn",
                        "pnpm" => "pnpm install",
                        "bun" => "bun install",
                        _ => "npm install",
                    };
                    let run_cmd = match pm.as_str() {
                        "yarn" => "yarn",
                        "pnpm" => "pnpm",
                        "bun" => "bun",
                        _ => "npx",
                    };
                    println!(
                        "  \x1b[90mcd {}\n  {}  # install dependencies\n  {} pledge dev\x1b[0m\n",
                        project_name, install_cmd, run_cmd
                    );
                } else {
                    println!("  \x1b[90mcd {} && pledge dev\x1b[0m\n", project_name);
                }
            }
        }

        Commands::Init { force, framework } => {
            let root = std::path::Path::new(".");

            // Check if pledge.config.ts already exists
            let config_exists = root.join("pledge.config.ts").exists()
                || root.join("pledge.config.js").exists()
                || root.join("pledge.config.json").exists()
                || root.join("pledge.json").exists();

            if config_exists && !force {
                println!("\n  \x1b[33mpledge config already exists\x1b[0m\n");
                println!("  Use \x1b[36mpledge init --force\x1b[0m to overwrite\n");
                return Ok(());
            }

            // Detect project framework
            let detection = detect::detect_project(root);

            let framework_name =
                framework.unwrap_or_else(|| detection.framework.as_str().to_string());

            println!("\n  \x1b[36mpledge init\x1b[0m — adding Pledgepack to your project\n");
            println!("  \x1b[90mDetected:\x1b[0m");
            println!("    Framework:      {}", framework_name);
            println!(
                "    TypeScript:     {}",
                if detection.typescript { "yes" } else { "no" }
            );
            println!(
                "    CSS:            {}",
                detection.css_preprocessor.as_str()
            );
            println!("    Package mgr:    {}", detection.package_manager.as_str());
            println!("    Build tool:     {}", detection.build_tool.as_str());
            println!("    Entry:          {}", detection.entry_file);
            println!();

            // Interactive confirm if in a TTY
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) && !force {
                let proceed = inquire::Confirm::new("Proceed with these settings?")
                    .with_default(true)
                    .prompt();
                match proceed {
                    Ok(true) => {}
                    Ok(false) => {
                        println!("\n  \x1b[33mCancelled.\x1b[0m\n");
                        return Ok(());
                    }
                    Err(_) => {}
                }
            }

            // Generate config
            let config_content = detect::generate_config(&detection);
            let config_path = root.join("pledge.config.ts");
            std::fs::write(&config_path, &config_content)?;
            println!("  \x1b[32m✓\x1b[0m Created pledge.config.ts");

            // Update package.json scripts
            let pkg_path = root.join("package.json");
            if pkg_path.exists() {
                let pkg_content = std::fs::read_to_string(&pkg_path)?;
                if let Ok(mut pkg) = serde_json::from_str::<serde_json::Value>(&pkg_content) {
                    if let Some(scripts) = pkg.get_mut("scripts").and_then(|s| s.as_object_mut()) {
                        scripts.insert(
                            "dev".to_string(),
                            serde_json::Value::String("pledge dev".to_string()),
                        );
                        scripts.insert(
                            "build".to_string(),
                            serde_json::Value::String("pledge build".to_string()),
                        );
                        scripts.insert(
                            "preview".to_string(),
                            serde_json::Value::String("pledge preview".to_string()),
                        );
                    }
                    let new_pkg = serde_json::to_string_pretty(&pkg)?;
                    std::fs::write(&pkg_path, new_pkg)?;
                    println!("  \x1b[32m✓\x1b[0m Updated package.json scripts");
                }
            }

            // Update .gitignore
            let gitignore_path = root.join(".gitignore");
            let gitignore = if gitignore_path.exists() {
                std::fs::read_to_string(&gitignore_path).unwrap_or_default()
            } else {
                String::new()
            };

            let mut new_gitignore = gitignore.clone();
            for entry in [".pledge/", "pledge-env.d.ts"] {
                if !new_gitignore.contains(entry) {
                    if !new_gitignore.is_empty() && !new_gitignore.ends_with('\n') {
                        new_gitignore.push('\n');
                    }
                    new_gitignore.push_str(entry);
                    new_gitignore.push('\n');
                }
            }
            if new_gitignore != gitignore {
                std::fs::write(&gitignore_path, &new_gitignore)?;
                println!("  \x1b[32m✓\x1b[0m Updated .gitignore");
            }

            // Create .env if it doesn't exist
            let env_path = root.join(".env");
            if !env_path.exists() {
                std::fs::write(
                    &env_path,
                    "# Pledge environment variables\nPLEDGE_APP_NAME=My App\n",
                )?;
                println!("  \x1b[32m✓\x1b[0m Created .env");
            }

            // Create index.html if it doesn't exist
            let html_path = root.join("index.html");
            if !html_path.exists() {
                let project_name = std::path::Path::new(".")
                    .canonicalize()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "My App".to_string());
                std::fs::write(
                    &html_path,
                    html::generate_default_html(&detection.entry_file, &project_name),
                )?;
                println!("  \x1b[32m✓\x1b[0m Created index.html");
            }

            println!("\n  \x1b[90mNext steps:\x1b[0m");
            println!("    {} pledgepack", detection.package_manager.install_cmd());
            println!("    {} pledge dev\n", detection.package_manager.dev_cmd());
        }

        Commands::Doctor => {
            println!("\n  \x1b[36mpledge doctor\x1b[0m — running diagnostics...\n");

            let root = std::path::Path::new(".");
            let report = doctor::run_diagnostics(root, &config);

            // Group by category
            let categories = [
                doctor::DiagnosticCategory::Config,
                doctor::DiagnosticCategory::Dependencies,
                doctor::DiagnosticCategory::Performance,
                doctor::DiagnosticCategory::Project,
                doctor::DiagnosticCategory::Security,
            ];

            for category in &categories {
                let checks: Vec<&doctor::DiagnosticCheck> = report
                    .checks
                    .iter()
                    .filter(|c| &c.category == category)
                    .collect();

                if checks.is_empty() {
                    continue;
                }

                println!("  \x1b[1m{}\x1b[0m", category.label());
                for check in checks {
                    print!("    {} {} ", check.status.icon(), check.name);
                    if check.status == doctor::DiagnosticStatus::Pass {
                        println!("\x1b[90m{}\x1b[0m", check.message);
                    } else {
                        println!("{}", check.message);
                    }
                    if let Some(ref suggestion) = check.suggestion {
                        println!("       \x1b[36m→ {}\x1b[0m", suggestion);
                    }
                }
                println!();
            }

            // Summary
            println!("  \x1b[1mSummary\x1b[0m");
            println!(
                "    {} passed, {} warnings, {} failed, {} info",
                report.summary.passed,
                report.summary.warnings,
                report.summary.failed,
                report.summary.info,
            );

            if report.summary.failed > 0 {
                println!(
                    "\n  \x1b[31m{} issue(s) need attention\x1b[0m\n",
                    report.summary.failed
                );
            } else if report.summary.warnings > 0 {
                println!(
                    "\n  \x1b[33m{} warning(s) — project should work but consider fixing\x1b[0m\n",
                    report.summary.warnings
                );
            } else {
                println!("\n  \x1b[32mAll checks passed!\x1b[0m\n");
            }

            // #83: Dependency vulnerability scanning
            println!("  \x1b[1mSecurity Audit\x1b[0m");
            let vulns = pledgepack_core::security::scan_vulnerabilities(root);
            println!(
                "{}",
                pledgepack_core::security::format_vulnerability_report(&vulns)
            );
            println!();

            // #84: License compliance checking
            println!("  \x1b[1mLicense Compliance\x1b[0m");
            let licenses = pledgepack_core::security::scan_licenses(root);
            if licenses.is_empty() {
                println!("  \x1b[90m— no node_modules found (run npm install first)\x1b[0m");
            } else {
                let result = pledgepack_core::security::check_license_compliance(
                    &licenses,
                    &[],                      // no whitelist by default
                    &["GPL-3.0", "AGPL-3.0"], // blacklist copyleft by default
                );
                println!(
                    "{}",
                    pledgepack_core::security::format_license_report(&result)
                );
            }
            println!();
        }

        Commands::Migrate { dry_run } => {
            println!("\n  \x1b[36mpledge migrate\x1b[0m — migrating config to Pledgepack\n");

            let root = std::path::Path::new(".");
            match migrate::migrate_config(root) {
                Ok(result) => {
                    println!("  \x1b[90mSource:\x1b[0m detected {}\n", result.config_path);

                    if !result.migrated_fields.is_empty() {
                        println!("  \x1b[32mMigrated fields:\x1b[0m");
                        for field in &result.migrated_fields {
                            println!("    • {}", field);
                        }
                    }

                    if !result.warnings.is_empty() {
                        println!("\n  \x1b[33mWarnings:\x1b[0m");
                        for warning in &result.warnings {
                            println!("    • {}", warning);
                        }
                    }

                    if dry_run {
                        println!("\n  \x1b[90mDry run — no files written\x1b[0m");
                        println!("\n  \x1b[1mGenerated pledge.config.ts:\x1b[0m\n");
                        println!("{}", result.config_content);
                    } else {
                        let out_path = root.join("pledge.config.ts");
                        std::fs::write(&out_path, &result.config_content)?;
                        println!("\n  \x1b[32m✓\x1b[0m Written to pledge.config.ts\n");
                        println!(
                            "  \x1b[90mNext: update package.json scripts to use `pledge dev` / `pledge build`\x1b[0m\n"
                        );
                    }
                }
                Err(e) => {
                    eprintln!("\n  \x1b[31m✗\x1b[0m {}\n", e);
                    eprintln!(
                        "  \x1b[90mSupported: vite.config.{{ts,js,mjs}}, webpack.config.{{ts,js,cjs,mjs}}, config-overrides.js, next.config.{{ts,js,mjs}}\x1b[0m\n"
                    );
                    anyhow::bail!("Migration failed: {}", e);
                }
            }
        }

        Commands::Config => {
            println!("\n  \x1b[36mpledge config\x1b[0m — validating configuration\n");

            let root = std::path::Path::new(".");

            // Try to read the raw config file for field-level validation
            let config_files = [
                root.join("pledge.config.ts"),
                root.join("pledge.config.js"),
                root.join("pledge.config.json"),
                root.join("pledge.json"),
            ];

            let mut found_config = false;
            for config_path in &config_files {
                if config_path.exists() {
                    found_config = true;
                    let content = std::fs::read_to_string(config_path)?;

                    // Try to parse as JSON (for .json files)
                    let config_json = if config_path
                        .extension()
                        .map(|e| e == "json")
                        .unwrap_or(false)
                    {
                        serde_json::from_str(&content).ok()
                    } else {
                        // For TS/JS files, try to extract the config object
                        pledgepack_core::config::PledgeConfig::parse_ts_config(&content)
                            .ok()
                            .and_then(|c| serde_json::to_value(&c).ok())
                    };

                    if let Some(ref json) = config_json {
                        let mut errors = config_validate::validate_config_json(json);
                        errors.extend(config_validate::validate_config_values(json));
                        if errors.is_empty() {
                            println!("  \x1b[32m✓\x1b[0m Config is valid — no issues found\n");
                        } else {
                            println!(
                                "  \x1b[33m{} validation issue(s) found:\x1b[0m\n",
                                errors.len()
                            );
                            print!("{}", config_validate::format_errors(&errors));
                        }
                    } else {
                        println!(
                            "  \x1b[33m⚠\x1b[0m Could not parse config for validation — check syntax\n"
                        );
                    }
                    break;
                }
            }

            if !found_config {
                println!("  \x1b[33m⚠\x1b[0m No config file found — using defaults\n");
                println!("  \x1b[90mRun `pledge init` to generate a config file\x1b[0m\n");
            }

            // Show current effective config
            println!("  \x1b[1mCurrent configuration:\x1b[0m\n");
            println!("    Entry:          {:?}", config.entry);
            println!("    Framework:      {:?}", config.framework);
            println!("    Out dir:        {:?}", config.out_dir);
            println!(
                "    Dev server:     {}:{}",
                config.dev_server.host, config.dev_server.port
            );
            println!(
                "    HMR:            {}",
                if config.dev_server.hmr {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!(
                "    Source maps:    {}",
                if config.source_maps {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!(
                "    Cache:          {} ({:?})",
                if config.cache.enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                config.cache.dir
            );
            println!("    Env prefix:     {:?}", config.env_prefix);
            println!("    Plugins:        {} configured", config.plugins.len());
            if let Some(ref edge) = config.edge_target {
                println!("    Edge target:    {}", edge);
            }
            println!();
        }

        Commands::Serve { port } => {
            let port = port.unwrap_or(4000);
            let out_dir = config.out_dir.clone();

            if !out_dir.exists() {
                println!(
                    "\n  \x1b[33mpledge serve\x1b[0m — .pledge/ not found. Run `pledge build` first.\n"
                );
                return Ok(());
            }

            println!(
                "\n  \x1b[36mpledge serve\x1b[0m — serving {} on http://localhost:{}\n",
                out_dir.display(),
                port
            );

            // SPA fallback: serve index.html for any route that doesn't match a static file
            let index_path = out_dir.join("index.html");
            let serve_dir = tower_http::services::ServeDir::new(&out_dir)
                .fallback(tower_http::services::ServeFile::new(index_path));
            let app = axum::Router::new().fallback_service(serve_dir);

            let addr = format!("127.0.0.1:{}", port);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }

        Commands::Cache { action } => match action {
            CacheAction::Clear => {
                let cache_dir = config.cache.dir.clone();
                if cache_dir.exists() {
                    std::fs::remove_dir_all(&cache_dir)?;
                    println!("\n  \x1b[32m✓\x1b[0m Cache cleared: {:?}\n", cache_dir);
                } else {
                    println!("\n  \x1b[90mCache directory does not exist\x1b[0m\n");
                }
            }
            CacheAction::Stats => {
                let cache_dir = config.cache.dir.clone();
                if cache_dir.exists() {
                    let entries = std::fs::read_dir(&cache_dir)?
                        .filter_map(|e| e.ok())
                        .count();
                    println!("\n  Cache: {:?}\n  Entries: {}\n", cache_dir, entries);
                } else {
                    println!("\n  \x1b[90mNo cache directory\x1b[0m\n");
                }
            }
        },

        Commands::Bench {
            baseline,
            threshold,
        } => {
            println!("\n  \x1b[36mpledge bench\x1b[0m — benchmarking build performance\n");

            config.mode = pledgepack_core::config::BuildMode::Production;

            let runs = 5;
            let mut times: Vec<u128> = Vec::new();

            for i in 0..runs {
                // Clear in-memory cache between runs by creating a new engine
                // (disk cache stays warm to measure incremental performance)
                let mut engine = BuildEngine::new(Arc::new(config.clone()));
                let result = engine.build().await?;

                let ms = result.duration_ms;
                times.push(ms);

                println!(
                    "  Run {}/{}: {} modules ({} cached) in {}ms",
                    i + 1,
                    runs,
                    result.modules_built,
                    result.modules_cached,
                    ms
                );
            }

            // Calculate statistics
            times.sort();
            let min = times.first().unwrap_or(&0);
            let max = times.last().unwrap_or(&0);
            let avg: u128 = times.iter().sum::<u128>() / times.len() as u128;
            let median = times[times.len() / 2];

            println!("\n  \x1b[32mBenchmark Results\x1b[0m ({} runs)\n", runs);

            let mut bench_table = comfy_table::Table::new();
            bench_table
                .load_preset(comfy_table::presets::UTF8_FULL)
                .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
                .set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
            bench_table
                .set_header(vec!["Metric", "Time (ms)"])
                .add_row(vec!["Min", &min.to_string()])
                .add_row(vec!["Max", &max.to_string()])
                .add_row(vec!["Avg", &avg.to_string()])
                .add_row(vec!["Median", &median.to_string()]);
            println!("{}", bench_table);
            println!();

            // Performance regression detection (#103)
            if let Some(ref baseline_ref) = baseline {
                let threshold = threshold.unwrap_or(10.0);
                let report = pledgepack_core::bench::compare_with_baseline(
                    &config.root,
                    baseline_ref,
                    median,
                    threshold,
                )?;

                if let Some(ref r) = report {
                    println!("{}", r.format());
                } else {
                    println!(
                        "  \x1b[32m✓ No regression\x1b[0m baseline={} median={}ms",
                        baseline_ref, median
                    );
                }
            }

            // Record benchmark result
            let git_ref = std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "current".to_string());

            let _ = pledgepack_core::bench::record_bench(
                &config.root,
                &git_ref,
                median,
                times.len(),
                0,
            );
        }

        Commands::Analyze { port, graph } => {
            println!("\n  \x1b[36mpledge analyze\x1b[0m — analyzing bundle...\n");

            config.mode = pledgepack_core::config::BuildMode::Production;

            let mut engine = BuildEngine::new(Arc::new(config.clone()));
            let _ = engine.build().await?;

            let analysis = pledgepack_core::analyzer::analyze_build(&engine)?;
            let html_output = if graph {
                // Generate interactive dependency graph (#104)
                let cycles = pledgepack_core::analyzer::detect_circular_deps(&analysis);
                if !cycles.is_empty() {
                    println!(
                        "  \x1b[33m⚠ {} circular dependency(s) detected:\x1b[0m",
                        cycles.len()
                    );
                    for cycle in &cycles {
                        println!("    \x1b[31m{}\x1b[0m", cycle.join(" → "));
                    }
                    println!();
                }
                pledgepack_core::analyzer::generate_dependency_graph_html(&analysis)
            } else {
                pledgepack_core::analyzer::generate_analysis_html(&analysis)
            };

            let port = port.unwrap_or(4200);

            println!(
                "  \x1b[32m✓\x1b[0m {} modules, {:.1}KB total",
                analysis.total_modules,
                analysis.total_transformed_size as f64 / 1024.0
            );
            println!(
                "\n  \x1b[90mAnalysis server: http://localhost:{}\x1b[0m\n",
                port
            );

            let app = axum::Router::new().route(
                "/",
                axum::routing::get(
                    move || async move { axum::response::Html(html_output.clone()) },
                ),
            );

            let addr = format!("127.0.0.1:{}", port);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }

        Commands::Test {
            pattern,
            watch,
            ui,
            visual,
            update_baselines,
        } => {
            println!("\n  \x1b[36mpledge test\x1b[0m — running tests...\n");

            let pattern = pattern.unwrap_or_else(|| {
                if config.test.include.len() == 1 {
                    config.test.include[0].clone()
                } else {
                    "**/*.{test,spec}.{ts,tsx,js,jsx}".to_string()
                }
            });
            let test_dir = config.root.join("src");

            // Find test files
            let mut test_files = Vec::new();
            collect_test_files(&test_dir, &pattern, &mut test_files)?;

            if test_files.is_empty() {
                println!(
                    "  \x1b[33mNo test files found matching: {}\x1b[0m\n",
                    pattern
                );
                return Ok(());
            }

            println!("  Found {} test file(s)\n", test_files.len());

            let mut all_summaries: Vec<(
                String,
                pledgepack_js_plugin_host::test_runner::TestSummary,
            )> = Vec::new();
            let mut total_passed = 0;
            let mut total_failed = 0;
            let mut total_skipped = 0;

            for test_file in &test_files {
                let rel = pledgepack_core::normalize_path(
                    test_file.strip_prefix(&config.root).unwrap_or(test_file),
                );

                let summary =
                    match pledgepack_js_plugin_host::test_runner::run_test_file_with_config(
                        test_file,
                        &config.test,
                        &config.root,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("  \x1b[31m✗\x1b[0m {} — error: {}", rel, e);
                            total_failed += 1;
                            continue;
                        }
                    };

                if summary.results.is_empty() {
                    println!("  \x1b[90m○ {} (no tests)\x1b[0m", rel);
                    total_skipped += 1;
                    continue;
                }

                // Print suite header
                if !summary.results.is_empty() {
                    println!("\n  \x1b[90m{}\x1b[0m", rel);
                }

                for result in &summary.results {
                    let suite_label = if result.suite.is_empty() {
                        String::new()
                    } else {
                        format!(" \x1b[90m{}\x1b[0m ›", result.suite)
                    };

                    match result.status {
                        pledgepack_js_plugin_host::test_runner::TestStatus::Passed => {
                            println!(
                                "    \x1b[32m✓\x1b[0m{} {} \x1b[90m({}ms)\x1b[0m",
                                suite_label, result.name, result.duration_ms
                            );
                        }
                        pledgepack_js_plugin_host::test_runner::TestStatus::Failed => {
                            println!("    \x1b[31m✗\x1b[0m{} {}", suite_label, result.name);
                            if let Some(err) = &result.error {
                                println!("      \x1b[31m  {}\x1b[0m", err);
                            }
                        }
                        pledgepack_js_plugin_host::test_runner::TestStatus::Skipped => {
                            println!(
                                "    \x1b[90m○\x1b[0m{} {} \x1b[90m(skipped)\x1b[0m",
                                suite_label, result.name
                            );
                        }
                    }
                }

                total_passed += summary.passed;
                total_failed += summary.failed;
                total_skipped += summary.skipped;
                all_summaries.push((rel, summary));
            }

            println!(
                "\n  \x1b[32mTests:\x1b[0m  {} passed, {} skipped, {} failed\n",
                total_passed, total_skipped, total_failed
            );

            // Coverage report
            if config.test.coverage {
                println!(
                    "  \x1b[90mCoverage report ({}):\x1b[0m",
                    config.test.coverage_reporter
                );
                println!(
                    "  \x1b[90m  Coverage data collected from {} file(s)\x1b[0m\n",
                    all_summaries.len()
                );
            }

            // #75: Visual regression testing
            if visual {
                println!("  \x1b[36mVisual regression testing\x1b[0m — screenshot comparison\n");

                let vr_config = pledgepack_core::visual_regression::VisualRegressionConfig {
                    enabled: true,
                    update_baselines,
                    ..Default::default()
                };

                // Use dev server port for capturing screenshots
                let port = config.dev_server.port;

                match pledgepack_core::visual_regression::run_visual_tests(&vr_config, port) {
                    Ok(report) => {
                        print!(
                            "\n{}",
                            pledgepack_core::visual_regression::format_visual_report(&report)
                        );

                        if report.failed > 0 {
                            // Generate HTML report
                            let html =
                                pledgepack_core::visual_regression::generate_visual_html_report(
                                    &report,
                                );
                            let report_path =
                                config.root.join(".pledge").join("visual-report.html");
                            std::fs::create_dir_all(report_path.parent().unwrap_or(&config.root))?;
                            std::fs::write(&report_path, &html)?;
                            println!(
                                "  \x1b[90mVisual report: {}\x1b[0m\n",
                                report_path.display()
                            );

                            if !update_baselines {
                                anyhow::bail!(
                                    "Visual regression: {} page(s) failed",
                                    report.failed
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("  \x1b[31m✗ Visual regression failed: {}\x1b[0m\n", e);
                    }
                }
            }

            // Exit non-zero when tests fail so CI can detect failures.
            // Watch and UI modes keep running instead of exiting.
            if total_failed > 0 && !watch && !ui {
                anyhow::bail!("{} test(s) failed", total_failed);
            }

            if watch {
                println!("  \x1b[90mWatch mode — press Ctrl+C to exit\x1b[0m\n");
                use notify::{EventKind, RecursiveMode, Watcher, recommended_watcher};
                use std::sync::mpsc::channel;
                use std::time::{Duration, Instant};

                let (tx, rx) = channel::<notify::Result<notify::Event>>();
                let mut watcher =
                    recommended_watcher(move |res: notify::Result<notify::Event>| {
                        let _ = tx.send(res);
                    })?;
                watcher.watch(&test_dir, RecursiveMode::Recursive)?;

                let debounce_ms = Duration::from_millis(300);
                let mut last_change: Option<Instant> = None;

                loop {
                    match rx.recv_timeout(Duration::from_millis(100)) {
                        Ok(Ok(event)) => {
                            let is_relevant = matches!(
                                event.kind,
                                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                            );
                            if !is_relevant {
                                continue;
                            }
                            let has_src = event.paths.iter().any(|p| {
                                let s = p.to_string_lossy();
                                !s.contains("node_modules")
                                    && !s.contains(".pledge")
                                    && !s.contains("target")
                            });
                            if has_src {
                                last_change = Some(Instant::now());
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::warn!("Watch error: {}", e);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if let Some(last) = last_change
                                && last.elapsed() >= debounce_ms
                            {
                                println!("\n  \x1b[36mpledge test\x1b[0m — re-running tests...\n");
                                total_passed = 0;
                                total_failed = 0;
                                total_skipped = 0;
                                for test_file in &test_files {
                                    let rel = pledgepack_core::normalize_path(
                                        test_file.strip_prefix(&config.root).unwrap_or(test_file),
                                    );
                                    let summary = match pledgepack_js_plugin_host::test_runner::run_test_file_with_config(
                                            test_file,
                                            &config.test,
                                            &config.root,
                                        ) {
                                            Ok(s) => s,
                                            Err(e) => {
                                                eprintln!("  \x1b[31m✗\x1b[0m {} — error: {}", rel, e);
                                                total_failed += 1;
                                                continue;
                                            }
                                        };
                                    for result in &summary.results {
                                        match result.status {
                                                pledgepack_js_plugin_host::test_runner::TestStatus::Passed => {
                                                    println!("  \x1b[32m✓\x1b[0m {} › {}", result.suite, result.name);
                                                }
                                                pledgepack_js_plugin_host::test_runner::TestStatus::Failed => {
                                                    println!("  \x1b[31m✗\x1b[0m {} › {}", result.suite, result.name);
                                                    if let Some(err) = &result.error {
                                                        println!("    \x1b[31m{}\x1b[0m", err);
                                                    }
                                                }
                                                pledgepack_js_plugin_host::test_runner::TestStatus::Skipped => {
                                                    println!("  \x1b[90m○\x1b[0m {} › {} (skipped)", result.suite, result.name);
                                                }
                                            }
                                    }
                                    total_passed += summary.passed;
                                    total_failed += summary.failed;
                                    total_skipped += summary.skipped;
                                }
                                println!(
                                    "\n  \x1b[32mTests:\x1b[0m  {} passed, {} skipped, {} failed\n",
                                    total_passed, total_skipped, total_failed
                                );
                                last_change = None;
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            break;
                        }
                    }
                }
            }

            if ui {
                println!("  \x1b[36mUI mode\x1b[0m — generating test report...\n");

                let html =
                    pledgepack_js_plugin_host::test_runner::generate_html_report(&all_summaries);
                let report_path = config.root.join(".pledge").join("test-report.html");
                std::fs::create_dir_all(report_path.parent().unwrap_or(&config.root))?;
                std::fs::write(&report_path, &html)?;

                println!(
                    "  \x1b[32m✓\x1b[0m Test report written to {}\n",
                    report_path.display()
                );

                // Serve the report on a local server
                let ui_port = 5174;
                let html_output = html.clone();
                let app = axum::Router::new().route(
                    "/",
                    axum::routing::get(
                        move || async move { axum::response::Html(html_output.clone()) },
                    ),
                );

                let addr = format!("127.0.0.1:{}", ui_port);
                println!("  \x1b[90mTest UI running at http://{}\x1b[0m\n", addr);

                // Auto-open browser
                #[cfg(target_os = "windows")]
                std::process::Command::new("cmd")
                    .args(["/C", "start", "", &format!("http://{}", addr)])
                    .spawn()
                    .ok();

                let listener = tokio::net::TcpListener::bind(&addr).await?;
                axum::serve(listener, app).await?;
            }
        }

        Commands::Dashboard { port } => {
            let port = port.unwrap_or(4300);
            println!("\n  \x1b[36mpledge dashboard\x1b[0m — build telemetry\n");

            let history = pledgepack_core::telemetry::BuildHistory::load(&config.root)?;

            if history.builds.is_empty() {
                println!("  \x1b[33mNo build history found\x1b[0m — run `pledge build` first\n");
                return Ok(());
            }

            let recent = history.recent(10);
            println!(
                "  \x1b[90m{} build(s) recorded\x1b[0m\n",
                history.builds.len()
            );
            for r in recent.iter().rev() {
                let status = if r.success {
                    "\x1b[32m✓\x1b[0m"
                } else {
                    "\x1b[31m✗\x1b[0m"
                };
                println!(
                    "    {} {}ms — {} modules, {:.0}% cache hit",
                    status,
                    r.duration_ms,
                    r.modules_built + r.modules_cached,
                    r.cache_hit_rate * 100.0,
                );
            }

            let html_output = pledgepack_core::telemetry::generate_dashboard_html(&history);
            println!("\n  \x1b[90mDashboard: http://localhost:{}\x1b[0m\n", port);

            let app = axum::Router::new().route(
                "/",
                axum::routing::get(
                    move || async move { axum::response::Html(html_output.clone()) },
                ),
            );

            let addr = format!("127.0.0.1:{}", port);
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            axum::serve(listener, app).await?;
        }

        Commands::GenerateEnvTypes => {
            println!(
                "\n  \x1b[36mpledge generate-env-types\x1b[0m — generating .d.ts for import.meta.env\n"
            );

            let env = EnvVars::load(&config.root, config.mode, &config.env_prefix);
            let dts = env.generate_dts(&config.env_prefix);

            let dts_path = config.root.join("pledge-env.d.ts");
            std::fs::write(&dts_path, &dts)?;

            println!("  \x1b[32m✓\x1b[0m Generated {}\n", dts_path.display());
        }

        Commands::Completions { shell } => {
            use clap::CommandFactory;
            use clap_complete::Shell;

            let shell_name = match shell {
                Some(s) => s,
                None => {
                    let options = vec!["bash", "zsh", "fish", "powershell", "elvish"];
                    let ans = inquire::Select::new("Select shell", options).prompt();
                    match ans {
                        Ok(s) => s.to_string(),
                        Err(_) => {
                            println!("\n  \x1b[33mCancelled.\x1b[0m\n");
                            return Ok(());
                        }
                    }
                }
            };

            let shell_enum = match shell_name.as_str() {
                "bash" => Shell::Bash,
                "zsh" => Shell::Zsh,
                "fish" => Shell::Fish,
                "powershell" | "pwsh" => Shell::PowerShell,
                "elvish" => Shell::Elvish,
                other => {
                    anyhow::bail!(
                        "Unknown shell: {}. Supported shells: bash, zsh, fish, powershell, elvish",
                        other
                    );
                }
            };

            let mut cmd = Cli::command();
            let bin_name = "pledge";
            println!(
                "\n  \x1b[36mpledge completions\x1b[0m — generating {} completions\n",
                shell_name
            );
            clap_complete::generate(shell_enum, &mut cmd, bin_name, &mut std::io::stdout());
            println!(
                "\n  \x1b[32m✓\x1b[0m Add the output to your shell's completion directory or source it in your rc file.\n"
            );
        }

        Commands::Manpages { output } => {
            use clap::CommandFactory;

            let out_dir = output.unwrap_or_else(|| PathBuf::from("."));
            std::fs::create_dir_all(&out_dir)?;

            let cmd = Cli::command();
            let bin_name = "pledge";

            println!(
                "\n  \x1b[36mpledge manpages\x1b[0m — generating man pages in {}\n",
                out_dir.display()
            );

            // Generate main man page
            let man = clap_mangen::Man::new(cmd.clone());
            let mut buffer: Vec<u8> = Vec::new();
            man.render(&mut buffer)?;
            let man_path = out_dir.join(format!("{}.1", bin_name));
            std::fs::write(&man_path, &buffer)?;
            println!("  \x1b[32m✓\x1b[0m {}", man_path.display());

            // Generate subcommand man pages
            for sub in cmd.get_subcommands() {
                let name = sub.get_name();
                let sub_man = clap_mangen::Man::new(sub.clone());
                let mut sub_buffer: Vec<u8> = Vec::new();
                if sub_man.render(&mut sub_buffer).is_ok() {
                    let sub_path = out_dir.join(format!("{}-{}.1", bin_name, name));
                    if std::fs::write(&sub_path, &sub_buffer).is_ok() {
                        println!("  \x1b[32m✓\x1b[0m {}", sub_path.display());
                    }
                }
            }

            println!(
                "\n  \x1b[32m✓\x1b[0m Man pages generated. Install to /usr/local/share/man/man1/\n"
            );
        }

        Commands::Schema { output } => {
            let schema = pledgepack_core::generate_config_schema()?;
            let pretty = serde_json::to_string_pretty(&schema)?;

            if let Some(path) = output {
                std::fs::write(&path, &pretty)?;
                println!(
                    "  \x1b[32m✓\x1b[0m JSON Schema written to {}",
                    path.display()
                );
            } else {
                println!("{}", pretty);
            }
        }

        Commands::Playground { port } => {
            let port = port.unwrap_or(8080);
            println!("\n  \x1b[36mpledge playground\x1b[0m — interactive transform REPL\n");
            pledgepack_core::playground::serve_playground(port)?;
        }

        Commands::Plugin { action } => match action {
            PluginAction::Search { query } => {
                println!("\n  \x1b[36mpledge plugin search\x1b[0m — searching npm registry...\n");
                let q = query.as_deref();
                match pledgepack_core::plugin_registry::search_plugins(q) {
                    Ok(plugins) => {
                        if plugins.is_empty() {
                            println!("  \x1b[33mNo plugins found\x1b[0m\n");
                        } else {
                            println!("  \x1b[32mFound {} plugin(s):\x1b[0m\n", plugins.len());
                            print!(
                                "{}",
                                pledgepack_core::plugin_registry::format_plugin_list(&plugins)
                            );
                        }
                    }
                    Err(e) => {
                        anyhow::bail!("Search failed: {}", e);
                    }
                }
            }
            PluginAction::Install { name, dev } => {
                println!("\n  \x1b[36mpledge plugin install\x1b[0m\n");
                if let Err(e) = pledgepack_core::plugin_registry::install_plugin(&name, dev) {
                    anyhow::bail!("Install failed: {}", e);
                }
            }
            PluginAction::List => {
                println!("\n  \x1b[36mpledge plugin list\x1b[0m — installed plugins\n");
                let root = std::path::Path::new(".");
                match pledgepack_core::plugin_registry::list_installed_plugins(root) {
                    Ok(plugins) => {
                        if plugins.is_empty() {
                            println!("  \x1b[90mNo plugins installed\x1b[0m\n");
                        } else {
                            print!(
                                "{}",
                                pledgepack_core::plugin_registry::format_plugin_list(&plugins)
                            );
                        }
                    }
                    Err(e) => {
                        anyhow::bail!("{}", e);
                    }
                }
            }
            PluginAction::Create {
                name,
                description,
                author,
            } => {
                println!("\n  \x1b[36mpledge plugin create\x1b[0m — scaffolding new plugin\n");

                let opts = pledgepack_core::plugin_template::PluginTemplateOptions {
                    name: name.clone(),
                    description: description
                        .unwrap_or_else(|| format!("PledgePack plugin: {}", name)),
                    author: author.unwrap_or_else(|| "Your Name".to_string()),
                    hooks: pledgepack_core::plugin_template::PluginHook::all(),
                };

                let out_dir = std::path::PathBuf::from(&name);
                match pledgepack_core::plugin_template::scaffold_plugin(&opts, &out_dir) {
                    Ok(()) => {
                        println!(
                            "  \x1b[32m✓\x1b[0m Plugin scaffolded: {}\n",
                            out_dir.display()
                        );
                        println!("  \x1b[90mcd {} && pledge dev\x1b[0m\n", name);
                    }
                    Err(e) => {
                        anyhow::bail!("Failed: {}", e);
                    }
                }
            }
            PluginAction::Docs { file, output } => {
                println!("\n  \x1b[36mpledge plugin docs\x1b[0m — generating API docs\n");

                let source = match std::fs::read_to_string(&file) {
                    Ok(s) => s,
                    Err(e) => {
                        anyhow::bail!("Cannot read {}: {}", file.display(), e);
                    }
                };

                match pledgepack_core::plugin_docs::generate_plugin_docs(&source) {
                    Ok(docs) => {
                        let markdown = pledgepack_core::plugin_docs::render_markdown(&docs);
                        if let Some(out_path) = output {
                            std::fs::write(&out_path, &markdown)?;
                            println!(
                                "  \x1b[32m✓\x1b[0m Docs written to {}\n",
                                out_path.display()
                            );
                        } else {
                            println!("{}", markdown);
                        }
                    }
                    Err(e) => {
                        anyhow::bail!("Failed: {}", e);
                    }
                }
            }
        },

        Commands::Why { module } => {
            println!(
                "\n  \x1b[36mpledge why\x1b[0m — analyzing why '{}' is in the bundle\n",
                module
            );

            let mut engine = BuildEngine::new(Arc::new(config.clone()));
            let _ = engine.build().await?;

            let analysis = pledgepack_core::analyzer::analyze_build(&engine)?;
            let chains = pledgepack_core::analyzer::find_import_chains(&analysis, &module);

            if chains.is_empty() {
                println!("  \x1b[33mModule '{}' not found in bundle\x1b[0m\n", module);
            } else {
                for (i, chain) in chains.iter().enumerate() {
                    println!("  \x1b[90mChain #{}:\x1b[0m", i + 1);
                    for (j, mod_name) in chain.iter().enumerate() {
                        let prefix = if j == 0 {
                            "  \x1b[36m→\x1b[0m "
                        } else {
                            "  \x1b[90m→\x1b[0m "
                        };
                        println!("{}{}", prefix, mod_name);
                    }
                    println!();
                }
            }
        }

        Commands::Clean { deep } => {
            println!("\n  \x1b[36mpledge clean\x1b[0m — cleaning build artifacts...\n");
            let dirs_to_clean = vec![
                ".pledge",
                ".pledge-cache",
                "node_modules/.pledge-cache",
                "dist",
                ".pledge/public",
            ];
            for dir in &dirs_to_clean {
                let path = std::path::Path::new(dir);
                if path.exists() {
                    println!("  \x1b[90mRemoving {}...\x1b[0m", dir);
                    if let Err(e) = std::fs::remove_dir_all(path) {
                        eprintln!("  \x1b[33m⚠\x1b[0m Failed to remove {}: {}", dir, e);
                    }
                }
            }
            if deep {
                let cache = std::path::Path::new("node_modules/.cache");
                if cache.exists() {
                    println!("  \x1b[90mRemoving node_modules/.cache...\x1b[0m");
                    let _ = std::fs::remove_dir_all(cache);
                }
            }
            println!("\n  \x1b[32m✓\x1b[0m Done.\n");
        }

        Commands::Update { version } => {
            println!("\n  \x1b[36mpledge update\x1b[0m — checking for updates...\n");

            let target_version = version.unwrap_or_else(|| "latest".to_string());

            // Check if installed via npm (global)
            let npm_result = std::process::Command::new("npm")
                .args(["list", "-g", "pledgepack", "--depth=0"])
                .output();

            if let Ok(output) = npm_result
                && output.status.success()
            {
                println!("  \x1b[90mUpdating via npm...\x1b[0m");
                let status = std::process::Command::new("npm")
                    .args(["install", "-g", &format!("pledgepack@{}", target_version)])
                    .status();
                if status.map(|s| s.success()).unwrap_or(false) {
                    println!(
                        "\n  \x1b[32m✓\x1b[0m Updated to pledgepack@{}\n",
                        target_version
                    );
                } else {
                    eprintln!(
                        "\n  \x1b[31m✗\x1b[0m Failed to update. Try: npm install -g pledgepack@{}\n",
                        target_version
                    );
                }
                return Ok(());
            }

            // Fallback: suggest manual update
            println!("  \x1b[33m⚠\x1b[0m Could not determine installation method.\n");
            println!("  \x1b[90mTo update manually:\x1b[0m");
            println!("    npm:    npm install -g pledgepack@{}", target_version);
            println!("    cargo:  cargo install pledgepack\n");
        }
    }

    Ok(())
}

/// Recursively copy a directory tree (for template caching)
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            // Skip node_modules and .pledge dirs in cache
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name == "node_modules" || name == ".pledge" || name == ".git")
            {
                continue;
            }
            copy_dir_recursive(&path, &dest_path)?;
        } else if path.is_file() {
            std::fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}

/// #8: Pre-warmed module graph — scan source files on create so dev server starts faster
fn prewarm_module_graph(project_dir: &std::path::Path, pledge_dir: &std::path::Path) {
    let mut modules: Vec<serde_json::Value> = Vec::new();

    fn scan_dir(
        dir: &std::path::Path,
        root: &std::path::Path,
        modules: &mut Vec<serde_json::Value>,
    ) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str())
                        && (name == "node_modules" || name.starts_with('.'))
                    {
                        continue;
                    }
                    scan_dir(&path, root, modules);
                } else if path.is_file() {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(
                        ext,
                        "ts" | "tsx"
                            | "js"
                            | "jsx"
                            | "css"
                            | "json"
                            | "vue"
                            | "svelte"
                            | "scss"
                            | "sass"
                    ) {
                        let rel = pledgepack_core::normalize_path(
                            path.strip_prefix(root).unwrap_or(&path),
                        );
                        let kind = pledgepack_core::module::ModuleKind::from_extension(&format!(
                            ".{}",
                            ext
                        ));
                        modules.push(serde_json::json!({
                            "path": rel,
                            "kind": format!("{:?}", kind),
                        }));
                    }
                }
            }
        }
    }

    scan_dir(project_dir, project_dir, &mut modules);

    let graph = serde_json::json!({
        "version": 1,
        "modules": modules,
    });

    let _ = std::fs::write(
        pledge_dir.join("module-graph.json"),
        serde_json::to_string_pretty(&graph).unwrap_or_default(),
    );
}

/// Recursively collect test files matching a pattern using globset
fn collect_test_files(
    dir: &std::path::Path,
    pattern: &str,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    // Build a GlobSet from the pattern (may contain brace expansion like {test,spec})
    let glob = match globset::Glob::new(pattern) {
        Ok(g) => g,
        Err(_) => {
            // Fall back to simple matching if glob pattern is invalid
            return collect_test_files_fallback(dir, pattern, files);
        }
    };
    let matcher = glob.compile_matcher();

    fn walk(dir: &std::path::Path, matcher: &globset::GlobMatcher, files: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                if path.is_dir() {
                    if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n == "node_modules" || n.starts_with('.'))
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    walk(&path, matcher, files);
                } else if path.is_file() {
                    let rel = path.strip_prefix(dir).unwrap_or(&path);
                    let rel_str = pledgepack_core::normalize_path(rel);
                    if matcher.is_match(&rel_str) {
                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        if matches!(ext, "ts" | "tsx" | "js" | "jsx") {
                            files.push(path);
                        }
                    }
                }
            }
        }
    }

    walk(dir, &matcher, files);
    Ok(())
}

/// Fallback test file collection using simple string matching
fn collect_test_files_fallback(
    dir: &std::path::Path,
    _pattern: &str,
    files: &mut Vec<PathBuf>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n == "node_modules" || n.starts_with('.'))
                .unwrap_or(false)
            {
                continue;
            }
            collect_test_files_fallback(&path, _pattern, files)?;
        } else if path.is_file() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.contains(".test.") || name.contains(".spec.") {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if matches!(ext, "ts" | "tsx" | "js" | "jsx") {
                    files.push(path);
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    // PRODUCTION-READINESS-100.md goal 89: this 3,800+-line entry point —
    // the primary way any user actually interacts with PledgePack — had
    // zero tests before this pass. `Cli`/`Commands`/`PluginAction`/
    // `CacheAction` don't derive `Debug`/`PartialEq`, so these tests match
    // on the parsed variants directly rather than comparing structs, but
    // that's a fine, arguably more precise way to assert "this exact flag
    // combination produces this exact parsed value" anyway.
    use super::*;
    use clap::Parser;

    /// Parse `pledge <args...>` the same way `main()` does (argv[0] is the
    /// program name clap expects but ignores for parsing purposes).
    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec!["pledge"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full)
    }

    // ── Global flags ──────────────────────────────────────────────────

    #[test]
    fn global_root_and_config_flags_are_optional_and_apply_to_subcommands() {
        let cli = parse(&["--root", "/tmp/proj", "--config", "custom.json", "doctor"]).unwrap();
        assert_eq!(cli.root.as_deref().map(|p| p.as_str()), Some("/tmp/proj"));
        assert_eq!(
            cli.config.as_deref().map(|p| p.as_str()),
            Some("custom.json")
        );
        assert!(matches!(cli.command, Commands::Doctor));
    }

    #[test]
    fn global_flags_default_to_none_when_omitted() {
        let cli = parse(&["doctor"]).unwrap();
        assert!(cli.root.is_none());
        assert!(cli.config.is_none());
    }

    #[test]
    fn no_subcommand_is_a_parse_error() {
        // `Commands` is a required subcommand (no #[command(subcommand)]
        // default) — omitting it entirely must fail to parse, not silently
        // pick something.
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn unknown_subcommand_is_a_parse_error() {
        assert!(parse(&["not-a-real-command"]).is_err());
    }

    // ── dev ───────────────────────────────────────────────────────────

    #[test]
    fn dev_defaults_when_no_flags_given() {
        let cli = parse(&["dev"]).unwrap();
        match cli.command {
            Commands::Dev {
                port,
                host,
                open,
                https,
                socket,
            } => {
                assert_eq!(port, None);
                assert_eq!(host, None);
                assert!(!open);
                assert!(!https);
                assert_eq!(socket, None);
            }
            _ => panic!("expected Commands::Dev"),
        }
    }

    #[test]
    fn dev_parses_short_and_long_flags() {
        let cli = parse(&[
            "dev", "-p", "4000", "--host", "0.0.0.0", "--open", "--https",
        ])
        .unwrap();
        match cli.command {
            Commands::Dev {
                port,
                host,
                open,
                https,
                ..
            } => {
                assert_eq!(port, Some(4000));
                assert_eq!(host.as_deref(), Some("0.0.0.0"));
                assert!(open);
                assert!(https);
            }
            _ => panic!("expected Commands::Dev"),
        }
    }

    #[test]
    fn dev_port_must_be_a_valid_u16() {
        assert!(parse(&["dev", "--port", "not-a-number"]).is_err());
        assert!(parse(&["dev", "--port", "99999999"]).is_err()); // exceeds u16::MAX
    }

    // ── build ─────────────────────────────────────────────────────────

    #[test]
    fn build_boolean_flags_default_false() {
        let cli = parse(&["build"]).unwrap();
        match cli.command {
            Commands::Build {
                no_sourcemap,
                profile,
                watch,
                verify,
                type_check,
                check_budgets,
                codegen,
                ..
            } => {
                assert!(!no_sourcemap);
                assert!(!profile);
                assert!(!watch);
                assert!(!verify);
                assert!(!type_check);
                assert!(!check_budgets);
                assert!(!codegen);
            }
            _ => panic!("expected Commands::Build"),
        }
    }

    #[test]
    fn build_accepts_out_dir_and_env_and_multiple_flags_together() {
        let cli = parse(&[
            "build",
            "--out-dir",
            "dist",
            "--env",
            "staging",
            "--watch",
            "--verify",
            "--type-check",
        ])
        .unwrap();
        match cli.command {
            Commands::Build {
                out_dir,
                env,
                watch,
                verify,
                type_check,
                ..
            } => {
                assert_eq!(out_dir.as_deref().map(|p| p.as_str()), Some("dist"));
                assert_eq!(env.as_deref(), Some("staging"));
                assert!(watch);
                assert!(verify);
                assert!(type_check);
            }
            _ => panic!("expected Commands::Build"),
        }
    }

    // ── create ────────────────────────────────────────────────────────

    #[test]
    fn create_with_no_args_leaves_template_and_name_unset() {
        let cli = parse(&["create"]).unwrap();
        match cli.command {
            Commands::Create {
                template,
                name,
                flash,
            } => {
                assert_eq!(template, None);
                assert_eq!(name, None);
                assert!(!flash);
            }
            _ => panic!("expected Commands::Create"),
        }
    }

    #[test]
    fn create_positional_args_map_to_template_then_name() {
        let cli = parse(&["create", "react", "my-app", "--flash"]).unwrap();
        match cli.command {
            Commands::Create {
                template,
                name,
                flash,
            } => {
                assert_eq!(template.as_deref(), Some("react"));
                assert_eq!(name.as_deref(), Some("my-app"));
                assert!(flash);
            }
            _ => panic!("expected Commands::Create"),
        }
    }

    // ── why (required positional arg) ────────────────────────────────

    #[test]
    fn why_requires_a_module_argument() {
        assert!(parse(&["why"]).is_err());
        let cli = parse(&["why", "lodash"]).unwrap();
        match cli.command {
            Commands::Why { module } => assert_eq!(module, "lodash"),
            _ => panic!("expected Commands::Why"),
        }
    }

    // ── nested subcommands: cache, plugin ────────────────────────────

    #[test]
    fn cache_requires_an_action_subcommand() {
        assert!(parse(&["cache"]).is_err());
        let cli = parse(&["cache", "clear"]).unwrap();
        match cli.command {
            Commands::Cache { action } => assert!(matches!(action, CacheAction::Clear)),
            _ => panic!("expected Commands::Cache"),
        }
        let cli = parse(&["cache", "stats"]).unwrap();
        match cli.command {
            Commands::Cache { action } => assert!(matches!(action, CacheAction::Stats)),
            _ => panic!("expected Commands::Cache"),
        }
    }

    #[test]
    fn plugin_install_requires_a_name_and_parses_dev_flag() {
        assert!(parse(&["plugin", "install"]).is_err()); // missing required `name`
        let cli = parse(&["plugin", "install", "@pledge/css-modules", "--dev"]).unwrap();
        match cli.command {
            Commands::Plugin { action } => match action {
                PluginAction::Install { name, dev } => {
                    assert_eq!(name, "@pledge/css-modules");
                    assert!(dev);
                }
                _ => panic!("expected PluginAction::Install"),
            },
            _ => panic!("expected Commands::Plugin"),
        }
    }

    #[test]
    fn plugin_search_query_is_optional() {
        let cli = parse(&["plugin", "search"]).unwrap();
        match cli.command {
            Commands::Plugin { action } => match action {
                PluginAction::Search { query } => assert_eq!(query, None),
                _ => panic!("expected PluginAction::Search"),
            },
            _ => panic!("expected Commands::Plugin"),
        }
    }

    // ── bench (optional f64 flag) ────────────────────────────────────

    #[test]
    fn bench_threshold_parses_as_f64() {
        let cli = parse(&["bench", "--threshold", "12.5"]).unwrap();
        match cli.command {
            Commands::Bench { threshold, .. } => assert_eq!(threshold, Some(12.5)),
            _ => panic!("expected Commands::Bench"),
        }
    }

    #[test]
    fn bench_threshold_rejects_non_numeric_value() {
        assert!(parse(&["bench", "--threshold", "fast"]).is_err());
    }

    // ── simple no-arg subcommands ─────────────────────────────────────

    #[test]
    fn parameterless_subcommands_parse() {
        assert!(matches!(
            parse(&["doctor"]).unwrap().command,
            Commands::Doctor
        ));
        assert!(matches!(
            parse(&["config"]).unwrap().command,
            Commands::Config
        ));
        assert!(matches!(
            parse(&["generate-env-types"]).unwrap().command,
            Commands::GenerateEnvTypes
        ));
    }

    // ── framework_deps / css_framework_dev_deps (pure helper functions) ─

    #[test]
    fn framework_deps_maps_known_templates_to_their_packages() {
        assert_eq!(framework_deps("react")["react"], template_versions::REACT);
        assert_eq!(
            framework_deps("solid")["solid-js"],
            template_versions::SOLID
        );
        assert_eq!(framework_deps("vue")["vue"], template_versions::VUE);
        assert_eq!(
            framework_deps("svelte")["svelte"],
            template_versions::SVELTE
        );
        // "pledge", "next", and "tanstack" all use the React runtime.
        assert_eq!(framework_deps("pledge")["react"], template_versions::REACT);
        assert_eq!(framework_deps("next")["react"], template_versions::REACT);
        assert_eq!(
            framework_deps("tanstack")["react"],
            template_versions::REACT
        );
    }

    #[test]
    fn framework_deps_unknown_template_returns_empty_object() {
        let deps = framework_deps("some-unknown-template");
        assert_eq!(deps, serde_json::json!({}));
    }

    #[test]
    fn css_framework_dev_deps_maps_known_choices() {
        let tailwind = css_framework_dev_deps("tailwind");
        assert_eq!(tailwind["tailwindcss"], template_versions::TAILWINDCSS);
        assert_eq!(tailwind["postcss"], template_versions::POSTCSS);
        assert_eq!(tailwind["autoprefixer"], template_versions::AUTOPREFIXER);

        assert_eq!(
            css_framework_dev_deps("unocss")["unocss"],
            template_versions::UNOCSS
        );
        assert_eq!(
            css_framework_dev_deps("panda-css")["@pandacss/dev"],
            template_versions::PANDA_CSS
        );
        assert_eq!(
            css_framework_dev_deps("vanilla-extract")["@vanilla-extract/css"],
            template_versions::VANILLA_EXTRACT
        );
    }

    #[test]
    fn css_framework_dev_deps_unknown_choice_returns_empty_object() {
        assert_eq!(css_framework_dev_deps("none"), serde_json::json!({}));
        assert_eq!(css_framework_dev_deps(""), serde_json::json!({}));
    }
}
