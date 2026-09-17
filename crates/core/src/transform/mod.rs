// Transform pipeline: Oxc + Lightning CSS integration
//
// This module dispatches transforms based on ModuleKind.
// Each file type has its own submodule:
//   - js.rs     — TypeScript/JSX/JavaScript via Oxc
//   - css.rs    — CSS, CSS Modules, Sass, Tailwind/PostCSS
//   - assets.rs — JSON, static assets, WASM, shaders
//   - sfc.rs    — Vue, Svelte, Astro Single-File Components
//   - env.rs    — Environment variables, define, import.meta.glob
//   - data.rs   — MDX, GraphQL, YAML, CSV, TSV, TOML
//   - utils.rs  — Source maps, Web Worker imports

mod assets;
mod css;
mod data;
mod env;
mod js;
mod sfc;
mod utils;

use crate::config::PledgeConfig;
use crate::module::ModuleKind;
use anyhow::Result;

pub use env::generate_env_dts;
pub use js::detect_dynamic_imports_from_program;

/// Output of transforming a single module
pub struct TransformOutput {
    pub code: String,
    pub source_map: Option<String>,
    /// CSS module class name mappings (original → scoped)
    pub css_modules: Option<Vec<(String, String)>>,
    /// Whether this module is CSS (for extraction)
    pub is_css: bool,
    /// Additional CSS extracted from SFCs (Vue/Svelte/Astro)
    pub extracted_css: Option<String>,
    /// Whether this is a worker module (for chunk splitting)
    pub is_worker: bool,
    /// Dynamic import specifiers found in this module
    pub dynamic_imports: Vec<String>,
    /// #75: Precomputed content hash (computed at transform time, not emit)
    pub content_hash: Option<String>,
}

/// Transform a module based on its kind
pub fn transform(
    source: &str,
    kind: ModuleKind,
    file_path: &str,
    is_production: bool,
    config: &PledgeConfig,
) -> Result<TransformOutput> {
    // Normalize line endings: files read on Windows may contain CRLF (\r\n),
    // which would skew parser spans, line/column info, and source maps.
    // All parsers below see LF-only source.
    let normalized;
    let source = if source.contains('\r') {
        normalized = source.replace("\r\n", "\n");
        normalized.as_str()
    } else {
        source
    };

    match kind {
        ModuleKind::TypeScript | ModuleKind::Tsx | ModuleKind::Jsx | ModuleKind::JavaScript => {
            js::transform_js(source, kind, file_path, is_production, config)
        }
        ModuleKind::Psx => {
            js::transform_js(source, ModuleKind::Tsx, file_path, is_production, config)
        }
        ModuleKind::Ps => Ok(TransformOutput {
            code: source.to_string(),
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        }),
        ModuleKind::Css => css::transform_css(source, file_path, is_production, config),
        ModuleKind::Json => assets::transform_json(source),
        ModuleKind::Asset => {
            assets::transform_asset(file_path, source.as_bytes(), is_production, config)
        }
        ModuleKind::Wasm => assets::transform_wasm(file_path, config),
        ModuleKind::Vue => sfc::transform_vue(source, file_path, is_production),
        ModuleKind::Svelte => sfc::transform_svelte(source, file_path, is_production),
        ModuleKind::Astro => sfc::transform_astro(source, file_path, is_production),
        ModuleKind::Worker => js::transform_js(source, kind, file_path, is_production, config),
        ModuleKind::SharedWorker => {
            js::transform_js(source, kind, file_path, is_production, config)
        }
        ModuleKind::WebComponent => {
            let code = crate::advanced::compile_web_component(source, file_path)?;
            Ok(TransformOutput {
                code,
                source_map: None,
                css_modules: None,
                is_css: false,
                extracted_css: None,
                is_worker: false,
                dynamic_imports: Vec::new(),
                content_hash: None,
            })
        }
        ModuleKind::Mdx => data::transform_mdx(source, file_path),
        ModuleKind::Graphql => data::transform_graphql(source, file_path),
        ModuleKind::Yaml => data::transform_yaml(source, file_path),
        ModuleKind::Csv => data::transform_csv(source, file_path),
        ModuleKind::Tsv => data::transform_tsv(source, file_path),
        ModuleKind::Sass => css::transform_sass(source, file_path, is_production, config),
        ModuleKind::Toml => data::transform_toml(source, file_path),
        ModuleKind::Shader => assets::transform_shader(source, file_path),
        _ => Ok(TransformOutput {
            code: source.to_string(),
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        }),
    }
}
