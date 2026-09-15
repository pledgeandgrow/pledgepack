// Performance & Optimization features: #71 route-based chunk splitting,
// #72 module prefetch directives, #73 CSS-in-JS runtime tree shaking,
// #74 WASM streaming compilation, #75 precompute module hash at transform.

use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;
use tracing::{info, warn};

// ── Feature 71: Route-based chunk splitting ───────────────────────────

#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub path: String,
    pub entry_module: String,
    pub lazy_imports: Vec<String>,
}

/// Detect route-based entry points from the app directory structure.
/// Scans for `app/` or `pages/` directory and maps file paths to routes.
pub fn detect_routes(root: &Path) -> Vec<RouteInfo> {
    let mut routes = Vec::new();

    for app_dir in &["app", "pages", "src/app", "src/pages"] {
        let dir = root.join(app_dir);
        if dir.is_dir() {
            scan_route_dir(&dir, app_dir, &mut routes);
        }
    }

    routes
}

fn scan_route_dir(dir: &Path, base: &str, routes: &mut Vec<RouteInfo>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                scan_route_dir(&path, base, routes);
            } else if let Some(ext) = path.extension()
                && matches!(
                    ext.to_str(),
                    Some("tsx") | Some("ts") | Some("jsx") | Some("js")
                )
            {
                let route_path = route_path_from_file(&path, base);
                let entry_module = path.to_string_lossy().to_string();
                routes.push(RouteInfo {
                    path: route_path,
                    entry_module,
                    lazy_imports: Vec::new(),
                });
            }
        }
    }
}

fn route_path_from_file(path: &Path, base: &str) -> String {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let parent = path.parent().and_then(|p| p.to_str()).unwrap_or("");

    // Normalize backslashes to forward slashes, then strip base prefix
    let parent_normalized = crate::normalize_path_str(parent);
    let route_segment = parent_normalized
        .strip_prefix(base)
        .unwrap_or(&parent_normalized)
        .to_string();

    let file_route = match file_name {
        "index.tsx" | "index.ts" | "index.jsx" | "index.js" => String::new(),
        f => {
            let stem = f.rsplit_once('.').map(|(s, _)| s).unwrap_or(f);
            if stem == "index" {
                String::new()
            } else {
                format!("/{}", stem)
            }
        }
    };

    let route = if route_segment.is_empty() || route_segment == "/" {
        file_route.clone()
    } else {
        format!("{}{}", route_segment, file_route)
    };

    if route.is_empty() {
        "/".to_string()
    } else {
        route
    }
}

/// Extract lazy/dynamic imports from source code.
pub fn extract_lazy_imports(source: &str) -> Vec<String> {
    let mut imports = Vec::new();

    static LAZY_IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    let re = LAZY_IMPORT_RE
        .get_or_init(|| Regex::new(r#"(?:import\s*\(|lazy\s*\()\s*['"]([^'"]+)['"]"#).unwrap());

    for cap in re.captures_iter(source) {
        imports.push(cap[1].to_string());
    }

    static DYNAMIC_IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    let re2 = DYNAMIC_IMPORT_RE
        .get_or_init(|| Regex::new(r#"import\s*\(\s*['"]([^'"]+)['"]\s*\)"#).unwrap());

    for cap in re2.captures_iter(source) {
        if !imports.contains(&cap[1].to_string()) {
            imports.push(cap[1].to_string());
        }
    }

    imports
}

/// Build route-aware chunk splitting configuration.
/// Analyzes route imports and groups shared modules into common chunks.
pub fn build_route_chunks(
    routes: &[RouteInfo],
    all_modules: &HashMap<String, Vec<String>>,
) -> RouteChunkConfig {
    let mut module_routes: HashMap<String, HashSet<String>> = HashMap::new();

    for route in routes {
        if let Some(mods) = all_modules.get(&route.entry_module) {
            for m in mods {
                module_routes
                    .entry(m.clone())
                    .or_default()
                    .insert(route.path.clone());
            }
        }
    }

    let mut shared_modules = Vec::new();
    let mut route_exclusive: HashMap<String, Vec<String>> = HashMap::new();

    for (module, routes_using) in &module_routes {
        if routes_using.len() > 1 {
            shared_modules.push(module.clone());
        } else {
            let route = routes_using.iter().next().unwrap();
            route_exclusive
                .entry(route.clone())
                .or_default()
                .push(module.clone());
        }
    }

    info!(
        "Route-based splitting: {} routes, {} shared modules, {} route-exclusive modules",
        routes.len(),
        shared_modules.len(),
        route_exclusive.values().map(|v| v.len()).sum::<usize>()
    );

    RouteChunkConfig {
        shared_chunk: shared_modules,
        route_chunks: route_exclusive,
    }
}

#[derive(Debug, Clone)]
pub struct RouteChunkConfig {
    pub shared_chunk: Vec<String>,
    pub route_chunks: HashMap<String, Vec<String>>,
}

// ── Feature 72: Module prefetch directives ────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchStrategy {
    Hover,
    Viewport,
    Load,
    None,
}

impl PrefetchStrategy {
    pub fn parse_strategy(s: &str) -> Self {
        match s {
            "hover" => Self::Hover,
            "viewport" => Self::Viewport,
            "load" => Self::Load,
            _ => Self::None,
        }
    }
}

/// Generate `<link rel="modulepreload">` tags for entry chunk dependencies.
pub fn generate_modulepreload_tags(chunk_files: &[String]) -> String {
    generate_modulepreload_tags_with_base(chunk_files, "/")
}

/// Generate `<link rel="modulepreload">` tags with base path prefix.
pub fn generate_modulepreload_tags_with_base(chunk_files: &[String], base: &str) -> String {
    let base = base.trim_end_matches('/');
    chunk_files
        .iter()
        .map(|f| format!(r#"<link rel="modulepreload" href="{}/{}">"#, base, f))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Generate `<link rel="prefetch">` tags for likely-next route chunks.
pub fn generate_prefetch_tags(
    route_chunks: &HashMap<String, Vec<String>>,
    current_route: &str,
    strategy: PrefetchStrategy,
) -> String {
    generate_prefetch_tags_with_base(route_chunks, current_route, strategy, "/")
}

/// Generate `<link rel="prefetch">` tags with base path prefix.
pub fn generate_prefetch_tags_with_base(
    route_chunks: &HashMap<String, Vec<String>>,
    current_route: &str,
    strategy: PrefetchStrategy,
    base: &str,
) -> String {
    if strategy == PrefetchStrategy::None {
        return String::new();
    }

    let base = base.trim_end_matches('/');
    let mut tags = Vec::new();

    // Find sibling/child routes that are likely to be navigated to next
    let candidates: Vec<&String> = route_chunks
        .keys()
        .filter(|route| {
            *route != current_route
                && (route.starts_with(current_route) || is_sibling_route(current_route, route))
        })
        .collect();

    for route in candidates {
        if let Some(files) = route_chunks.get(route) {
            for file in files {
                let tag = match strategy {
                    PrefetchStrategy::Load => {
                        format!(
                            r#"<link rel="prefetch" href="{}/{}" as="script">"#,
                            base, file
                        )
                    }
                    PrefetchStrategy::Viewport => {
                        format!(
                            r#"<link rel="prefetch" href="{}/{}" as="script" data-prefetch-strategy="viewport">"#,
                            base, file
                        )
                    }
                    PrefetchStrategy::Hover => {
                        format!(
                            r#"<link rel="prefetch" href="{}/{}" as="script" data-prefetch-strategy="hover" data-prefetch-route="{}">"#,
                            base, file, route
                        )
                    }
                    _ => continue,
                };
                tags.push(tag);
            }
        }
    }

    tags.join("\n")
}

fn is_sibling_route(a: &str, b: &str) -> bool {
    let a_parts: Vec<&str> = a.split('/').filter(|s| !s.is_empty()).collect();
    let b_parts: Vec<&str> = b.split('/').filter(|s| !s.is_empty()).collect();

    if a_parts.is_empty() || b_parts.is_empty() {
        return false;
    }

    a_parts.len() == b_parts.len() && a_parts[..a_parts.len() - 1] == b_parts[..b_parts.len() - 1]
}

// ── Feature 73: CSS-in-JS runtime tree shaking ────────────────────────

/// Strip CSS-in-JS runtime imports after static extraction.
/// Removes styled-components/emotion runtime imports when all styles
/// have been extracted at build time.
pub fn strip_css_in_js_runtime(source: &str, framework: &str) -> String {
    let mut result = source.to_string();

    match framework {
        "styled-components" => {
            result = strip_styled_components_runtime(&result);
        }
        "emotion" => {
            result = strip_emotion_runtime(&result);
        }
        "vanilla-extract" => {
            result = strip_vanilla_extract_runtime(&result);
        }
        _ => {}
    }

    result
}

fn strip_styled_components_runtime(source: &str) -> String {
    static IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    let re = IMPORT_RE.get_or_init(|| {
        Regex::new(r#"import\s+[^;]*\s+from\s+['"]styled-components['"];?\n?"#).unwrap()
    });

    let result = re.replace_all(source, "").to_string();

    if result != source {
        info!("Stripped styled-components runtime import (zero-runtime CSS-in-JS)");
    }

    result
}

fn strip_emotion_runtime(source: &str) -> String {
    static IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    let re = IMPORT_RE.get_or_init(|| {
        Regex::new(r#"import\s+[^;]*\s+from\s+['"]@emotion/[^'"]+['"];?\n?"#).unwrap()
    });

    let result = re.replace_all(source, "").to_string();

    if result != source {
        info!("Stripped @emotion runtime imports (zero-runtime CSS-in-JS)");
    }

    result
}

fn strip_vanilla_extract_runtime(source: &str) -> String {
    static IMPORT_RE: OnceLock<Regex> = OnceLock::new();
    let re = IMPORT_RE.get_or_init(|| {
        Regex::new(r#"import\s+[^;]*\s+from\s+['"]@vanilla-extract/[^'"]+['"];?\n?"#).unwrap()
    });

    let result = re.replace_all(source, "").to_string();

    if result != source {
        info!("Stripped @vanilla-extract runtime imports (zero-runtime CSS-in-JS)");
    }

    result
}

/// Check if all CSS-in-JS styles were statically extractable.
/// If so, the runtime can be safely removed.
pub fn can_strip_runtime(extracted_css: &str, dynamic_styles: &[String]) -> bool {
    if extracted_css.is_empty() {
        return false;
    }

    if !dynamic_styles.is_empty() {
        warn!(
            "Cannot strip CSS-in-JS runtime: {} dynamic style(s) detected",
            dynamic_styles.len()
        );
        return false;
    }

    true
}

// ── Feature 74: WASM streaming compilation ────────────────────────────

/// Generate WASM instantiation code using WebAssembly.streaming().
/// Falls back to buffer-based instantiation for older browsers.
pub fn generate_wasm_streaming_code(url: &str, fallback: bool) -> String {
    if fallback {
        format!(
            r#"export default async function() {{
  // WASM streaming compilation (#74)
  if (typeof WebAssembly.instantiateStreaming === 'function') {{
    const {{ instance }} = await WebAssembly.instantiateStreaming(
      fetch("{url}", {{ headers: {{ "Content-Type": "application/wasm" }} }}),
      {{}}
    );
    return instance.exports;
  }}
  // Fallback for older browsers
  const response = await fetch("{url}");
  const bytes = new Uint8Array(await response.arrayBuffer());
  const {{ instance }} = await WebAssembly.instantiate(bytes, {{}});
  return instance.exports;
}}"#,
            url = url
        )
    } else {
        format!(
            r#"export default async function() {{
  const {{ instance }} = await WebAssembly.instantiateStreaming(
    fetch("{url}", {{ headers: {{ "Content-Type": "application/wasm" }} }}),
    {{}}
  );
  return instance.exports;
}}"#,
            url = url
        )
    }
}

/// Check if a WASM file should use streaming compilation.
pub fn should_use_streaming(file_size: usize) -> bool {
    // Streaming is beneficial for larger WASM files
    file_size > 1024
}

// ── Feature 75: Precompute module hash at transform time ──────────────

/// Compute a content hash for a module during the transform pass.
/// This eliminates the need for a separate hash computation during emit.
#[derive(Debug, Clone, Default)]
pub struct ModuleHash {
    /// Content hash (hex string)
    pub hash: String,
    /// Module path
    pub path: String,
    /// Whether the hash was computed during transform
    pub computed_at_transform: bool,
}

/// Compute a fast content hash for module content.
pub fn compute_module_hash(content: &str, file_path: &str) -> ModuleHash {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    file_path.hash(&mut hasher);
    // Extra mixing for better distribution
    let h1 = hasher.finish();
    let mut hasher2 = std::collections::hash_map::DefaultHasher::new();
    h1.hash(&mut hasher2);
    content.len().hash(&mut hasher2);
    let h2 = hasher2.finish();

    let hash = format!("{:016x}{:016x}", h1, h2);

    ModuleHash {
        hash,
        path: file_path.to_string(),
        computed_at_transform: true,
    }
}

/// Generate a content-addressed filename from a module hash.
pub fn hash_filename(original: &str, hash: &str) -> String {
    let ext = original
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("js");
    let stem = original.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let file_stem = original
        .rsplit('/')
        .next()
        .unwrap_or(original)
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(original);

    let short_hash = &hash[..8];

    if stem.is_empty() {
        format!("{}.{}.{}", file_stem, short_hash, ext)
    } else {
        format!("{}/{}.{}.{}", stem, file_stem, short_hash, ext)
    }
}

/// Hash map for all transformed modules.
pub type ModuleHashMap = HashMap<String, ModuleHash>;

/// Batch compute hashes for all transformed modules.
pub fn batch_compute_hashes(modules: &[(String, String)]) -> ModuleHashMap {
    modules
        .iter()
        .map(|(path, content)| {
            let hash = compute_module_hash(content, path);
            (path.clone(), hash)
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_lazy_imports() {
        let source = r#"
const About = lazy(() => import("./About"));
const data = import("./data.json");
const mod = await import("./mod.ts");
"#;
        let imports = extract_lazy_imports(source);
        assert!(imports.contains(&"./About".to_string()));
        assert!(imports.contains(&"./data.json".to_string()));
        assert!(imports.contains(&"./mod.ts".to_string()));
    }

    #[test]
    fn test_route_path_from_file() {
        let path = Path::new("app/index.tsx");
        assert_eq!(route_path_from_file(path, "app"), "/");

        let path = Path::new("app/about/index.tsx");
        assert_eq!(route_path_from_file(path, "app"), "/about");

        let path = Path::new("app/users/[id].tsx");
        let route = route_path_from_file(path, "app");
        assert!(route.contains("users"));
    }

    #[test]
    fn test_generate_modulepreload_tags() {
        let tags = generate_modulepreload_tags(&["vendor.js".to_string(), "shared.js".to_string()]);
        assert!(tags.contains("modulepreload"));
        assert!(tags.contains("vendor.js"));
        assert!(tags.contains("shared.js"));
    }

    #[test]
    fn test_generate_prefetch_tags_hover() {
        let mut route_chunks = HashMap::new();
        route_chunks.insert("/about".to_string(), vec!["about.js".to_string()]);
        route_chunks.insert("/users".to_string(), vec!["users.js".to_string()]);

        let tags = generate_prefetch_tags(&route_chunks, "/", PrefetchStrategy::Hover);
        assert!(tags.contains("prefetch"));
        assert!(tags.contains("data-prefetch-strategy=\"hover\""));
    }

    #[test]
    fn test_strip_styled_components_runtime() {
        let source = "import styled from 'styled-components';\nconst Box = \"div\";";
        let result = strip_styled_components_runtime(source);
        assert!(!result.contains("styled-components"));
        assert!(result.contains("const Box"));
    }

    #[test]
    fn test_strip_emotion_runtime() {
        let source = "import { css } from '@emotion/react';\nconst s = \"abc\";";
        let result = strip_emotion_runtime(source);
        assert!(!result.contains("@emotion"));
    }

    #[test]
    fn test_can_strip_runtime() {
        assert!(can_strip_runtime(".btn { color: red; }", &[]));
        assert!(!can_strip_runtime("", &[]));
        assert!(!can_strip_runtime(
            ".btn { color: red; }",
            &["dynamic".to_string()]
        ));
    }

    #[test]
    fn test_wasm_streaming_code() {
        let code = generate_wasm_streaming_code("/module.wasm", true);
        assert!(code.contains("instantiateStreaming"));
        assert!(code.contains("fetch(\"/module.wasm\""));
        assert!(code.contains("Fallback"));
    }

    #[test]
    fn test_wasm_streaming_no_fallback() {
        let code = generate_wasm_streaming_code("/module.wasm", false);
        assert!(code.contains("instantiateStreaming"));
        assert!(!code.contains("Fallback"));
    }

    #[test]
    fn test_compute_module_hash() {
        let hash = compute_module_hash("console.log('hello')", "test.js");
        assert!(!hash.hash.is_empty());
        assert_eq!(hash.hash.len(), 32);
        assert!(hash.computed_at_transform);
    }

    #[test]
    fn test_hash_filename() {
        let name = hash_filename("src/button.js", "abcdef1234567890");
        assert!(name.contains("abcdef12"));
        assert!(name.ends_with(".js"));
    }

    #[test]
    fn test_batch_compute_hashes() {
        let modules = vec![
            ("a.js".to_string(), "console.log('a')".to_string()),
            ("b.js".to_string(), "console.log('b')".to_string()),
        ];
        let hashes = batch_compute_hashes(&modules);
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains_key("a.js"));
        assert!(hashes.contains_key("b.js"));
    }

    #[test]
    fn test_is_sibling_route() {
        assert!(is_sibling_route("/about", "/contact"));
        assert!(!is_sibling_route("/about", "/about/team"));
        assert!(!is_sibling_route("/", "/about"));
    }

    #[test]
    fn test_should_use_streaming() {
        assert!(!should_use_streaming(512));
        assert!(should_use_streaming(2048));
    }
}
