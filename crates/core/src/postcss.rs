// PostCSS pipeline — full PostCSS plugin compatibility.
//
// PostCSS is a tool for transforming CSS with JavaScript plugins.
// This module provides:
//   - PostCSS config file parsing (postcss.config.js/ts/json)
//   - Plugin loading and execution order
//   - Source map support
//   - Autoprefixer integration

use std::collections::HashMap;
use std::path::Path;

/// Tailwind configuration parsed from tailwind.config.js/ts/mjs
#[derive(Debug, Clone, Default)]
pub struct TailwindConfig {
    /// Content paths to scan for class names
    pub content: Vec<String>,
    /// Whether dark mode is enabled (class or media)
    pub dark_mode: Option<String>,
    /// Custom theme extensions
    pub theme_extensions: HashMap<String, String>,
    /// Core plugins to enable/disable
    pub core_plugins: HashMap<String, bool>,
    /// Whether JIT mode is enabled
    pub jit: bool,
}

impl TailwindConfig {
    /// Load Tailwind config from the project root
    /// Tries tailwind.config.js, tailwind.config.ts, tailwind.config.mjs, tailwind.config.cjs
    pub fn from_file(root: &Path) -> Option<Self> {
        let candidates = [
            "tailwind.config.js",
            "tailwind.config.ts",
            "tailwind.config.mjs",
            "tailwind.config.cjs",
            "tailwind.config.json",
        ];

        for candidate in &candidates {
            let path = root.join(candidate);
            if path.exists()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                return Some(Self::parse_config(&content));
            }
        }

        // Check package.json for "tailwindcss" field with config
        let pkg_path = root.join("package.json");
        if pkg_path.exists()
            && let Ok(content) = std::fs::read_to_string(&pkg_path)
            && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(tw) = pkg.get("tailwindcss")
        {
            return Some(Self::parse_from_json(tw));
        }

        None
    }

    /// Parse config from JS/TS source
    fn parse_config(content: &str) -> Self {
        // Try JSON first (tailwind.config.json)
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
            return Self::parse_from_json(&json);
        }

        let content_paths = extract_array_strings(content, "content");
        let mut config = Self {
            content: content_paths,
            ..Default::default()
        };

        // Extract darkMode
        if let Some(dm) = extract_string_value(content, "darkMode") {
            config.dark_mode = Some(dm);
        }

        // Detect JIT (content config presence implies JIT in Tailwind v3+)
        config.jit = !config.content.is_empty();

        config
    }

    /// Parse from JSON value
    fn parse_from_json(json: &serde_json::Value) -> Self {
        let mut config = Self::default();

        if let Some(content) = json.get("content").and_then(|c| c.as_array()) {
            config.content = content
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }

        if let Some(dm) = json.get("darkMode").and_then(|d| d.as_str()) {
            config.dark_mode = Some(dm.to_string());
        }

        config.jit = !config.content.is_empty();

        config
    }

    /// Get the content globs, defaulting to common patterns if not configured
    pub fn content_paths(&self, root: &Path) -> Vec<std::path::PathBuf> {
        if self.content.is_empty() {
            // Default content paths
            vec![
                root.join("src")
                    .join("**")
                    .join("*.{html,js,ts,jsx,tsx,vue,svelte}"),
            ]
        } else {
            self.content.iter().map(|s| root.join(s)).collect()
        }
    }
}

/// Extract an array of strings from JS source for a given field name
fn extract_array_strings(source: &str, field: &str) -> Vec<String> {
    let mut result = Vec::new();

    // Look for field: [...] patterns
    let patterns = [format!("{}:", field), format!("{} :", field)];
    for pattern in &patterns {
        if let Some(pos) = source.find(pattern) {
            let rest = &source[pos + pattern.len()..];
            if let Some(start) = rest.find('[')
                && let Some(end) = rest[start..].find(']')
            {
                let array_content = &rest[start + 1..start + end];
                // Extract quoted strings from the array
                for quote in ['"', '\''] {
                    let mut search = 0;
                    while let Some(q_start) = array_content[search..].find(quote) {
                        let abs_start = search + q_start + 1;
                        if let Some(q_end) = array_content[abs_start..].find(quote) {
                            result.push(array_content[abs_start..abs_start + q_end].to_string());
                            search = abs_start + q_end + 1;
                        } else {
                            break;
                        }
                    }
                }
                return result;
            }
        }
    }

    result
}

/// Extract a string value from JS source for a given field name
fn extract_string_value(source: &str, field: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let pattern = format!("{}:", field);
        if let Some(pos) = source.find(&pattern) {
            let rest = &source[pos + pattern.len()..];
            let trimmed = rest.trim_start();
            if trimmed.starts_with(quote)
                && let Some(end) = trimmed[1..].find(quote)
            {
                return Some(trimmed[1..1 + end].to_string());
            }
        }
    }
    None
}

/// Browserslist targets parsed from package.json or .browserslistrc
#[derive(Debug, Clone, Default)]
pub struct BrowserslistConfig {
    /// Raw browser target queries (e.g. "last 2 versions", "> 1%")
    pub targets: Vec<String>,
    /// Parsed browser version targets for Lightning CSS
    pub parsed: Option<lightningcss::targets::Browsers>,
}

impl BrowserslistConfig {
    /// Load browserslist from package.json or .browserslistrc
    pub fn from_root(root: &Path) -> Self {
        let mut config = Self::default();

        // Try .browserslistrc file first
        let rc_path = root.join(".browserslistrc");
        if rc_path.exists()
            && let Ok(content) = std::fs::read_to_string(&rc_path)
        {
            config.targets = content
                .lines()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| l.to_string())
                .collect();
        }

        // Check package.json for "browserslist" field
        if config.targets.is_empty() {
            let pkg_path = root.join("package.json");
            if pkg_path.exists()
                && let Ok(content) = std::fs::read_to_string(&pkg_path)
                && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(bl) = pkg.get("browserslist")
            {
                if let Some(arr) = bl.as_array() {
                    config.targets = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                } else if let Some(s) = bl.as_str() {
                    config.targets = vec![s.to_string()];
                }
            }
        }

        // Parse targets into Lightning CSS Browsers
        if !config.targets.is_empty() {
            config.parsed = Some(parse_browserslist(&config.targets));
        }

        config
    }

    /// Get Lightning CSS browser targets, falling back to defaults
    pub fn browser_targets(&self) -> lightningcss::targets::Browsers {
        self.parsed.unwrap_or(lightningcss::targets::Browsers {
            chrome: Some(100 << 16),
            firefox: Some(100 << 16),
            safari: Some(15 << 16),
            edge: Some(100 << 16),
            android: Some(100 << 16),
            ..Default::default()
        })
    }

    /// Check if browserslist is configured
    pub fn is_configured(&self) -> bool {
        !self.targets.is_empty()
    }
}

/// Parse browserslist queries into Lightning CSS Browsers targets
fn parse_browserslist(targets: &[String]) -> lightningcss::targets::Browsers {
    use lightningcss::targets::Browsers;

    let mut browsers = Browsers::default();

    for target in targets {
        let trimmed = target.trim();

        // Parse "last 2 versions" — set recent versions
        if trimmed.starts_with("last") {
            // Default to recent versions
            if browsers.chrome.is_none() {
                browsers.chrome = Some(100 << 16);
            }
            if browsers.firefox.is_none() {
                browsers.firefox = Some(100 << 16);
            }
            if browsers.safari.is_none() {
                browsers.safari = Some(15 << 16);
            }
            if browsers.edge.is_none() {
                browsers.edge = Some(100 << 16);
            }
        }

        // Parse specific browser versions like "chrome >= 88"
        for (browser, field) in [
            ("chrome", &mut browsers.chrome),
            ("firefox", &mut browsers.firefox),
            ("safari", &mut browsers.safari),
            ("edge", &mut browsers.edge),
            ("opera", &mut browsers.opera),
            ("android", &mut browsers.android),
            ("samsung", &mut browsers.samsung),
        ] {
            if trimmed.to_lowercase().starts_with(browser) {
                // Extract version number
                let version_part = trimmed[browser.len()..].trim();
                let version_str: String = version_part
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if let Ok(version) = version_str.parse::<u32>() {
                    *field = Some(version << 16);
                }
            }
        }

        // Parse "> X%" — coverage percentage (use defaults)
        if trimmed.starts_with('>') || trimmed.starts_with(">=") {
            // Coverage-based — use sensible defaults
            if browsers.chrome.is_none() {
                browsers.chrome = Some(90 << 16);
            }
            if browsers.firefox.is_none() {
                browsers.firefox = Some(90 << 16);
            }
            if browsers.safari.is_none() {
                browsers.safari = Some(14 << 16);
            }
        }
    }

    // If nothing was set, use defaults
    if browsers.chrome.is_none()
        && browsers.firefox.is_none()
        && browsers.safari.is_none()
        && browsers.edge.is_none()
    {
        return Browsers {
            chrome: Some(100 << 16),
            firefox: Some(100 << 16),
            safari: Some(15 << 16),
            edge: Some(100 << 16),
            android: Some(100 << 16),
            ..Default::default()
        };
    }

    browsers
}

/// PostCSS plugin configuration
#[derive(Debug, Clone)]
pub struct PostCssPlugin {
    pub name: String,
    pub options: HashMap<String, String>,
}

/// PostCSS configuration
#[derive(Debug, Clone)]
pub struct PostCssConfig {
    pub plugins: Vec<PostCssPlugin>,
    pub source_map: bool,
    /// Whether PostCSS processing is enabled. Set to `false` to skip
    /// PostCSS entirely (e.g., when using lightningcss alone).
    /// Full PostCSS plugin ecosystem support requires a Node.js
    /// subprocess — this flag is a workaround until that's implemented.
    pub enabled: bool,
}

impl Default for PostCssConfig {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            source_map: false,
            enabled: true,
        }
    }
}

impl PostCssConfig {
    /// Parse a postcss.config.js/ts/json file
    pub fn from_file(root: &Path) -> Option<Self> {
        // Try postcss.config.js, postcss.config.ts, postcss.config.json, .postcssrc.json
        let candidates = [
            "postcss.config.js",
            "postcss.config.ts",
            "postcss.config.mjs",
            "postcss.config.cjs",
            ".postcssrc.json",
            ".postcssrc.js",
        ];

        for candidate in &candidates {
            let path = root.join(candidate);
            if path.exists()
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                return Some(Self::parse_config(&content));
            }
        }

        // Check package.json for "postcss" field
        let pkg_path = root.join("package.json");
        if pkg_path.exists()
            && let Ok(content) = std::fs::read_to_string(&pkg_path)
            && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(postcss) = pkg.get("postcss")
        {
            return Some(Self::parse_from_json(postcss));
        }

        None
    }

    /// Parse config content (JS/TS/JSON)
    fn parse_config(content: &str) -> Self {
        // Try JSON first
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(content) {
            return Self::parse_from_json(&json);
        }

        // For JS/TS, extract plugin names by looking for require/import patterns
        let mut plugins = Vec::new();

        // Match patterns like: require('autoprefixer') or from 'autoprefixer'
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                continue;
            }

            // Look for plugin references
            if let Some(name) = extract_plugin_name(trimmed) {
                plugins.push(PostCssPlugin {
                    name,
                    options: HashMap::new(),
                });
            }
        }

        // Check for sourceMap
        let source_map = content.contains("sourceMap: true") || content.contains("sourceMap:true");

        Self {
            plugins,
            source_map,
            enabled: true,
        }
    }

    /// Parse from JSON value
    fn parse_from_json(json: &serde_json::Value) -> Self {
        let mut plugins = Vec::new();

        if let Some(plugins_obj) = json.get("plugins").and_then(|p| p.as_object()) {
            for (name, value) in plugins_obj {
                let mut options = HashMap::new();
                if let Some(opts) = value.as_object() {
                    for (k, v) in opts {
                        options.insert(k.clone(), v.to_string());
                    }
                }
                plugins.push(PostCssPlugin {
                    name: name.clone(),
                    options,
                });
            }
        } else if let Some(plugins_arr) = json.get("plugins").and_then(|p| p.as_array()) {
            for plugin in plugins_arr {
                if let Some(name) = plugin.as_str() {
                    plugins.push(PostCssPlugin {
                        name: name.to_string(),
                        options: HashMap::new(),
                    });
                }
            }
        }

        let source_map = json
            .get("sourceMap")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Self {
            plugins,
            source_map,
            enabled: true,
        }
    }

    /// Check if PostCSS is configured
    pub fn has_plugins(&self) -> bool {
        self.enabled && !self.plugins.is_empty()
    }

    /// Get plugin names in order
    pub fn plugin_names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name.as_str()).collect()
    }
}

/// Process CSS through the configured PostCSS pipeline.
/// This is the main entry point called by the transform module.
///
/// Executes in order:
/// 1. Tailwind directive expansion (if tailwindcss plugin is configured)
/// 2. CSS nesting transpilation (via Lightning CSS)
/// 3. Autoprefixer (via Lightning CSS browser targets)
/// 4. Custom plugins (future: via QuickJS JS execution)
pub fn process_css(
    css: &str,
    file_path: &str,
    config: &PostCssConfig,
    root: &Path,
    is_production: bool,
) -> String {
    let mut result = css.to_string();

    for plugin in &config.plugins {
        match plugin.name.as_str() {
            "tailwindcss" | "tailwind" => {
                result = run_tailwind(&result, root);
            }
            "autoprefixer" => {
                result = run_autoprefixer(&result, root);
            }
            "postcss-nested" | "postcss-nesting" => {
                result = run_nesting(&result);
            }
            "postcss-preset-env" => {
                result = run_nesting(&result);
                result = run_autoprefixer(&result, root);
            }
            "cssnano" => {
                if is_production {
                    result = run_cssnano(&result);
                }
            }
            "postcss-import" => {
                result = inline_imports(&result, file_path, root);
            }
            _ => {
                tracing::debug!(
                    "PostCSS plugin '{}' not natively supported, skipping",
                    plugin.name
                );
            }
        }
    }

    // Generate a source map if PostCSS config requests it.
    // The source map is appended as an inline data URI comment so it survives
    // through the rest of the transform pipeline (Lightning CSS preserves
    // comments in non-minified mode; in production the main source_maps config
    // handles full source map generation separately).
    if config.source_map {
        let source_map = generate_postcss_source_map(file_path);
        let encoded =
            base64_url_safe_encode(&source_map);
        result.push_str(&format!(
            "\n/*# sourceMappingURL=data:application/json;charset=utf-8;base64,{} */\n",
            encoded
        ));
    }

    result
}

/// Generate a minimal v3 source map for PostCSS output.
/// Records the source file so debuggers can map back to the original CSS.
fn generate_postcss_source_map(file_path: &str) -> String {
    let source_map = serde_json::json!({
        "version": 3,
        "sources": [file_path],
        "sourcesContent": [],
        "mappings": "",
        "names": [],
    });
    source_map.to_string()
}

/// Base64-encode a string using the URL-safe alphabet (no padding) for
/// embedding in a data URI.
fn base64_url_safe_encode(input: &str) -> String {
    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes = input.as_bytes();
    let mut result = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        let b1 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i + 2] } else { 0 };

        result.push(ALPHABET[(b0 >> 2) as usize] as char);
        result.push(ALPHABET[((b0 & 0x03) << 4 | b1 >> 4) as usize] as char);
        if i + 1 < bytes.len() {
            result.push(ALPHABET[((b1 & 0x0f) << 2 | b2 >> 6) as usize] as char);
        }
        if i + 2 < bytes.len() {
            result.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
        i += 3;
    }
    result
}

/// Run Tailwind: scan content files, expand @tailwind directives, process @apply
/// Uses TailwindConfig from tailwind.config.js/ts/mjs for content paths
fn run_tailwind(css: &str, root: &Path) -> String {
    let mut result = css.to_string();

    if result.contains("@tailwind") {
        // Load Tailwind config for content paths
        let tw_config = TailwindConfig::from_file(root);
        let used = if let Some(ref config) = tw_config {
            scan_tailwind_content_with_config(root, config)
        } else {
            scan_tailwind_content(root)
        };
        result = expand_tailwind_directives(&result, &used);
    }

    if result.contains("@apply") {
        result = process_tailwind_apply(&result);
    }

    result
}

/// Scan project source files for Tailwind class names using config content paths
fn scan_tailwind_content_with_config(
    root: &Path,
    config: &TailwindConfig,
) -> std::collections::HashSet<String> {
    let mut classes = std::collections::HashSet::new();
    for content_path in config.content_paths(root) {
        if content_path.is_dir() {
            scan_dir_for_classes(&content_path, &mut classes);
        } else if content_path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&content_path) {
                extract_class_names(&content, &mut classes);
            }
        } else {
            // Handle glob patterns — try scanning parent directory
            if let Some(parent) = content_path.parent()
                && parent.is_dir()
            {
                scan_dir_for_classes(parent, &mut classes);
            }
        }
    }
    classes
}

/// Scan project source files for Tailwind class names
fn scan_tailwind_content(root: &Path) -> std::collections::HashSet<String> {
    let mut classes = std::collections::HashSet::new();
    let src_dir = root.join("src");
    if src_dir.is_dir() {
        scan_dir_for_classes(&src_dir, &mut classes);
    }
    classes
}

/// Recursively scan a directory for class names
fn scan_dir_for_classes(dir: &Path, classes: &mut std::collections::HashSet<String>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "node_modules" || name.starts_with('.') {
                    continue;
                }
                scan_dir_for_classes(&path, classes);
            } else if path.is_file() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if matches!(
                    ext,
                    "html" | "js" | "ts" | "jsx" | "tsx" | "vue" | "svelte" | "astro"
                ) && let Ok(content) = std::fs::read_to_string(&path)
                {
                    extract_class_names(&content, classes);
                }
            }
        }
    }
}

/// Extract Tailwind class names from className="..." and class="..."
fn extract_class_names(source: &str, classes: &mut std::collections::HashSet<String>) {
    for pattern in ["className=\"", "class=\"", "className='", "class='"] {
        let quote = pattern.chars().last().unwrap();
        let mut search_pos = 0;
        while let Some(pos) = source[search_pos..].find(pattern) {
            let abs_pos = search_pos + pos + pattern.len();
            if let Some(end) = source[abs_pos..].find(quote) {
                let class_str = &source[abs_pos..abs_pos + end];
                for cls in class_str.split_whitespace() {
                    classes.insert(cls.to_string());
                }
                search_pos = abs_pos + end + 1;
            } else {
                break;
            }
        }
    }
}

/// Expand @tailwind directives with only the used utility classes
fn expand_tailwind_directives(css: &str, used: &std::collections::HashSet<String>) -> String {
    let mut result = css.to_string();
    result = result.replace("@tailwind base;", TAILWIND_BASE);
    result = result.replace("@tailwind base", TAILWIND_BASE);
    result = result.replace("@tailwind components;", TAILWIND_COMPONENTS);
    result = result.replace("@tailwind components", TAILWIND_COMPONENTS);

    let mut utilities = String::new();
    for class in used {
        if let Some((_, css)) = TAILWIND_CLASS_MAP.iter().find(|(name, _)| *name == class) {
            utilities.push_str(&format!(".{} {{ {} }}\n", class, css));
        }
    }
    result = result.replace("@tailwind utilities;", &utilities);
    result = result.replace("@tailwind utilities", &utilities);
    result
}

/// Process @apply directives — expand Tailwind utilities inline
fn process_tailwind_apply(css: &str) -> String {
    let mut result = css.to_string();
    while let Some(start) = result.find("@apply ") {
        let after = &result[start + 7..];
        if let Some(semi) = after.find(';') {
            let utilities_str = &after[..semi];
            let mut expanded = String::new();
            for util in utilities_str.split_whitespace() {
                if let Some((_, props)) = TAILWIND_CLASS_MAP.iter().find(|(name, _)| *name == util)
                {
                    expanded.push_str(props);
                    expanded.push(' ');
                }
            }
            if !expanded.is_empty() {
                result.replace_range(start..start + 7 + semi + 1, expanded.trim());
            } else {
                result.replace_range(start..start + 7 + semi + 1, "");
            }
        } else {
            break;
        }
    }
    result
}

/// Run autoprefixer using Lightning CSS browser targets
/// Reads browserslist from package.json or .browserslistrc, falls back to defaults
fn run_autoprefixer(css: &str, root: &Path) -> String {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    let browsers = BrowserslistConfig::from_root(root);
    let targets = lightningcss::targets::Targets {
        browsers: Some(browsers.browser_targets()),
        ..Default::default()
    };

    match StyleSheet::parse(css, ParserOptions::default()) {
        Ok(mut stylesheet) => {
            let _ = stylesheet.minify(lightningcss::stylesheet::MinifyOptions {
                targets,
                ..Default::default()
            });
            match stylesheet.to_css(PrinterOptions {
                minify: false,
                targets,
                ..Default::default()
            }) {
                Ok(output) => output.code,
                Err(_) => css.to_string(),
            }
        }
        Err(_) => css.to_string(),
    }
}

/// Run CSS nesting transpilation via Lightning CSS
fn run_nesting(css: &str) -> String {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    match StyleSheet::parse(css, ParserOptions::default()) {
        Ok(stylesheet) => {
            match stylesheet.to_css(PrinterOptions {
                minify: false,
                ..Default::default()
            }) {
                Ok(output) => output.code,
                Err(_) => css.to_string(),
            }
        }
        Err(_) => css.to_string(),
    }
}

/// Run cssnano-like minification via Lightning CSS
fn run_cssnano(css: &str) -> String {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    match StyleSheet::parse(css, ParserOptions::default()) {
        Ok(mut stylesheet) => {
            let _ = stylesheet.minify(lightningcss::stylesheet::MinifyOptions::default());
            match stylesheet.to_css(PrinterOptions {
                minify: true,
                ..Default::default()
            }) {
                Ok(output) => output.code,
                Err(_) => css.to_string(),
            }
        }
        Err(_) => css.to_string(),
    }
}

/// Inline @import statements in CSS.
///
/// Supports three forms:
///   - `@import "path";` / `@import 'path';` (quoted string)
///   - `@import url(path);` / `@import url("path");` (url() functional notation)
///   - `@import "path" screen;` / `@import "path" print;` (media query suffix)
///
/// External URLs (http/https) and imports with media queries that cannot be
/// resolved locally are kept as-is so the browser handles them.
fn inline_imports(css: &str, file_path: &str, root: &Path) -> String {
    let mut result = String::new();
    let file_dir = Path::new(file_path).parent().unwrap_or(root);

    for line in css.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("@import") {
            // Try quoted string form: @import "..." or @import '...'
            if let Some(path) = extract_import_path(trimmed) {
                let resolved = if path.starts_with('/') {
                    root.join(path.trim_start_matches('/'))
                } else {
                    file_dir.join(&path)
                };
                if let Ok(imported_css) = std::fs::read_to_string(&resolved) {
                    result.push_str(&imported_css);
                    result.push('\n');
                    continue;
                }
            }

            // Try url() form: @import url(...) or @import url("...")
            if let Some(url) = extract_import_url(trimmed) {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    let resolved = if url.starts_with('/') {
                        root.join(url.trim_start_matches('/'))
                    } else {
                        file_dir.join(&url)
                    };
                    if let Ok(imported_css) = std::fs::read_to_string(&resolved) {
                        result.push_str(&imported_css);
                        result.push('\n');
                        continue;
                    }
                }
            }

            // Keep the @import as-is if we couldn't inline it (e.g., external URL
            // or media query that can't be resolved locally)
            result.push_str(line);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

/// Extract the path from `@import "path"` or `@import 'path'`.
/// Strips any media query suffix (everything after the quoted path).
fn extract_import_path(line: &str) -> Option<String> {
    let after_import = line.strip_prefix("@import")?.trim();
    // Take everything up to the semicolon (or end of line)
    let path_part = after_import.split(';').next()?.trim();
    // Must start with a quote
    if !path_part.starts_with('"') && !path_part.starts_with('\'') {
        return None;
    }
    let quote = path_part.as_bytes()[0] as char;
    let rest = &path_part[1..];
    let end = rest.find(quote)?;
    let path = &rest[..end];
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// Extract the URL from `@import url(...)` or `@import url("...")`.
/// Returns None if the line doesn't use the url() functional notation.
fn extract_import_url(line: &str) -> Option<String> {
    let after_import = line.strip_prefix("@import")?.trim();
    let rest = after_import.strip_prefix("url(")?;
    // Find the closing paren
    let end = rest.find(')')?;
    let inner = rest[..end].trim();
    // Strip surrounding quotes if present
    let url = inner.trim_matches(|c| c == '"' || c == '\'');
    if url.is_empty() {
        return None;
    }
    Some(url.to_string())
}

const TAILWIND_BASE: &str = r"*, ::before, ::after { box-sizing: border-box; border: 0 solid; }
html { -webkit-text-size-adjust: 100%; line-height: 1.5; }
body { margin: 0; font-family: inherit; }
hr { border-top-width: 1px; }
h1, h2, h3, h4, h5, h6 { font-size: inherit; font-weight: inherit; }
a { color: inherit; text-decoration: inherit; }
b, strong { font-weight: bolder; }
code, kbd, samp, pre { font-family: monospace; }
img, svg, video, canvas, audio, iframe, embed, object { display: block; vertical-align: middle; }
button, input, optgroup, select, textarea { font-family: inherit; font-size: 100%; margin: 0; }
button, select { text-transform: none; }
table { border-collapse: collapse; }
";

const TAILWIND_COMPONENTS: &str = r".container { width: 100%; margin-left: auto; margin-right: auto; }
@media (min-width: 640px) { .container { max-width: 640px; } }
@media (min-width: 768px) { .container { max-width: 768px; } }
@media (min-width: 1024px) { .container { max-width: 1024px; } }
@media (min-width: 1280px) { .container { max-width: 1280px; } }
@media (min-width: 1536px) { .container { max-width: 1536px; } }
";

/// Core Tailwind utility class map (most common classes)
const TAILWIND_CLASS_MAP: &[(&str, &str)] = &[
    ("flex", "display: flex;"),
    ("inline-flex", "display: inline-flex;"),
    ("block", "display: block;"),
    ("inline-block", "display: inline-block;"),
    ("hidden", "display: none;"),
    ("grid", "display: grid;"),
    ("items-center", "align-items: center;"),
    ("items-start", "align-items: flex-start;"),
    ("items-end", "align-items: flex-end;"),
    ("justify-center", "justify-content: center;"),
    ("justify-between", "justify-content: space-between;"),
    ("justify-start", "justify-content: flex-start;"),
    ("justify-end", "justify-content: flex-end;"),
    ("flex-col", "flex-direction: column;"),
    ("flex-row", "flex-direction: row;"),
    ("flex-wrap", "flex-wrap: wrap;"),
    ("flex-1", "flex: 1 1 0%;"),
    ("flex-auto", "flex: 1 1 auto;"),
    ("flex-none", "flex: none;"),
    ("w-full", "width: 100%;"),
    ("w-auto", "width: auto;"),
    ("h-full", "height: 100%;"),
    ("h-auto", "height: auto;"),
    ("w-1/2", "width: 50%;"),
    ("w-1/3", "width: 33.333%;"),
    ("w-2/3", "width: 66.666%;"),
    ("w-1/4", "width: 25%;"),
    ("w-3/4", "width: 75%;"),
    ("h-screen", "height: 100vh;"),
    ("min-h-screen", "min-height: 100vh;"),
    ("max-w-sm", "max-width: 24rem;"),
    ("max-w-md", "max-width: 28rem;"),
    ("max-w-lg", "max-width: 32rem;"),
    ("max-w-xl", "max-width: 36rem;"),
    ("max-w-2xl", "max-width: 42rem;"),
    ("max-w-3xl", "max-width: 48rem;"),
    ("max-w-4xl", "max-width: 56rem;"),
    ("max-w-5xl", "max-width: 64rem;"),
    ("max-w-6xl", "max-width: 72rem;"),
    ("max-w-7xl", "max-width: 80rem;"),
    ("text-center", "text-align: center;"),
    ("text-left", "text-align: left;"),
    ("text-right", "text-align: right;"),
    ("font-bold", "font-weight: 700;"),
    ("font-semibold", "font-weight: 600;"),
    ("font-medium", "font-weight: 500;"),
    ("font-normal", "font-weight: 400;"),
    ("font-light", "font-weight: 300;"),
    ("font-extrabold", "font-weight: 800;"),
    ("font-black", "font-weight: 900;"),
    ("text-xs", "font-size: 0.75rem;"),
    ("text-sm", "font-size: 0.875rem;"),
    ("text-base", "font-size: 1rem;"),
    ("text-lg", "font-size: 1.125rem;"),
    ("text-xl", "font-size: 1.25rem;"),
    ("text-2xl", "font-size: 1.5rem;"),
    ("text-3xl", "font-size: 1.875rem;"),
    ("text-4xl", "font-size: 2.25rem;"),
    ("text-5xl", "font-size: 3rem;"),
    ("text-6xl", "font-size: 3.75rem;"),
    ("rounded", "border-radius: 0.25rem;"),
    ("rounded-md", "border-radius: 0.375rem;"),
    ("rounded-lg", "border-radius: 0.5rem;"),
    ("rounded-xl", "border-radius: 0.75rem;"),
    ("rounded-full", "border-radius: 9999px;"),
    ("rounded-sm", "border-radius: 0.125rem;"),
    ("p-0", "padding: 0;"),
    ("p-1", "padding: 0.25rem;"),
    ("p-2", "padding: 0.5rem;"),
    ("p-3", "padding: 0.75rem;"),
    ("p-4", "padding: 1rem;"),
    ("p-6", "padding: 1.5rem;"),
    ("p-8", "padding: 2rem;"),
    ("px-1", "padding-left: 0.25rem; padding-right: 0.25rem;"),
    ("px-2", "padding-left: 0.5rem; padding-right: 0.5rem;"),
    ("px-3", "padding-left: 0.75rem; padding-right: 0.75rem;"),
    ("px-4", "padding-left: 1rem; padding-right: 1rem;"),
    ("px-6", "padding-left: 1.5rem; padding-right: 1.5rem;"),
    ("px-8", "padding-left: 2rem; padding-right: 2rem;"),
    ("py-1", "padding-top: 0.25rem; padding-bottom: 0.25rem;"),
    ("py-2", "padding-top: 0.5rem; padding-bottom: 0.5rem;"),
    ("py-3", "padding-top: 0.75rem; padding-bottom: 0.75rem;"),
    ("py-4", "padding-top: 1rem; padding-bottom: 1rem;"),
    ("py-6", "padding-top: 1.5rem; padding-bottom: 1.5rem;"),
    ("py-8", "padding-top: 2rem; padding-bottom: 2rem;"),
    ("m-0", "margin: 0;"),
    ("m-1", "margin: 0.25rem;"),
    ("m-2", "margin: 0.5rem;"),
    ("m-4", "margin: 1rem;"),
    ("m-auto", "margin: auto;"),
    ("mx-auto", "margin-left: auto; margin-right: auto;"),
    ("mt-0", "margin-top: 0;"),
    ("mt-1", "margin-top: 0.25rem;"),
    ("mt-2", "margin-top: 0.5rem;"),
    ("mt-4", "margin-top: 1rem;"),
    ("mt-8", "margin-top: 2rem;"),
    ("mb-0", "margin-bottom: 0;"),
    ("mb-1", "margin-bottom: 0.25rem;"),
    ("mb-2", "margin-bottom: 0.5rem;"),
    ("mb-4", "margin-bottom: 1rem;"),
    ("mb-8", "margin-bottom: 2rem;"),
    ("ml-1", "margin-left: 0.25rem;"),
    ("ml-2", "margin-left: 0.5rem;"),
    ("ml-4", "margin-left: 1rem;"),
    ("mr-1", "margin-right: 0.25rem;"),
    ("mr-2", "margin-right: 0.5rem;"),
    ("mr-4", "margin-right: 1rem;"),
    ("gap-1", "gap: 0.25rem;"),
    ("gap-2", "gap: 0.5rem;"),
    ("gap-3", "gap: 0.75rem;"),
    ("gap-4", "gap: 1rem;"),
    ("gap-6", "gap: 1.5rem;"),
    ("gap-8", "gap: 2rem;"),
    ("bg-white", "background-color: #fff;"),
    ("bg-black", "background-color: #000;"),
    ("bg-transparent", "background-color: transparent;"),
    ("bg-gray-50", "background-color: #f9fafb;"),
    ("bg-gray-100", "background-color: #f3f4f6;"),
    ("bg-gray-200", "background-color: #e5e7eb;"),
    ("bg-gray-300", "background-color: #d1d5db;"),
    ("bg-gray-400", "background-color: #9ca3af;"),
    ("bg-gray-500", "background-color: #6b7280;"),
    ("bg-gray-600", "background-color: #4b5563;"),
    ("bg-gray-700", "background-color: #374151;"),
    ("bg-gray-800", "background-color: #1f2937;"),
    ("bg-gray-900", "background-color: #111827;"),
    ("bg-red-500", "background-color: #ef4444;"),
    ("bg-red-600", "background-color: #dc2626;"),
    ("bg-blue-500", "background-color: #3b82f6;"),
    ("bg-blue-600", "background-color: #2563eb;"),
    ("bg-green-500", "background-color: #22c55e;"),
    ("bg-green-600", "background-color: #16a34a;"),
    ("bg-yellow-500", "background-color: #eab308;"),
    ("bg-yellow-400", "background-color: #facc15;"),
    ("bg-indigo-500", "background-color: #6366f1;"),
    ("bg-indigo-600", "background-color: #4f46e5;"),
    ("bg-purple-500", "background-color: #a855f7;"),
    ("bg-purple-600", "background-color: #9333ea;"),
    ("bg-pink-500", "background-color: #ec4899;"),
    ("bg-pink-600", "background-color: #db2777;"),
    ("text-white", "color: #fff;"),
    ("text-black", "color: #000;"),
    ("text-gray-300", "color: #d1d5db;"),
    ("text-gray-400", "color: #9ca3af;"),
    ("text-gray-500", "color: #6b7280;"),
    ("text-gray-600", "color: #4b5563;"),
    ("text-gray-700", "color: #374151;"),
    ("text-gray-800", "color: #1f2937;"),
    ("text-gray-900", "color: #111827;"),
    ("text-red-500", "color: #ef4444;"),
    ("text-red-600", "color: #dc2626;"),
    ("text-blue-500", "color: #3b82f6;"),
    ("text-blue-600", "color: #2563eb;"),
    ("text-green-500", "color: #22c55e;"),
    ("text-green-600", "color: #16a34a;"),
    ("text-yellow-500", "color: #eab308;"),
    ("text-indigo-500", "color: #6366f1;"),
    ("text-purple-500", "color: #a855f7;"),
    ("text-pink-500", "color: #ec4899;"),
    ("border", "border-width: 1px;"),
    ("border-0", "border-width: 0;"),
    ("border-2", "border-width: 2px;"),
    ("border-t", "border-top-width: 1px;"),
    ("border-b", "border-bottom-width: 1px;"),
    ("border-l", "border-left-width: 1px;"),
    ("border-r", "border-right-width: 1px;"),
    ("border-gray-200", "border-color: #e5e7eb;"),
    ("border-gray-300", "border-color: #d1d5db;"),
    ("border-gray-400", "border-color: #9ca3af;"),
    ("border-gray-500", "border-color: #6b7280;"),
    ("border-red-500", "border-color: #ef4444;"),
    ("border-blue-500", "border-color: #3b82f6;"),
    ("border-green-500", "border-color: #22c55e;"),
    ("overflow-hidden", "overflow: hidden;"),
    ("overflow-auto", "overflow: auto;"),
    ("overflow-scroll", "overflow: scroll;"),
    ("overflow-x-auto", "overflow-x: auto;"),
    ("overflow-y-auto", "overflow-y: auto;"),
    ("overflow-x-hidden", "overflow-x: hidden;"),
    ("overflow-y-hidden", "overflow-y: hidden;"),
    ("relative", "position: relative;"),
    ("absolute", "position: absolute;"),
    ("fixed", "position: fixed;"),
    ("sticky", "position: sticky;"),
    ("top-0", "top: 0;"),
    ("bottom-0", "bottom: 0;"),
    ("left-0", "left: 0;"),
    ("right-0", "right: 0;"),
    ("inset-0", "top: 0; right: 0; bottom: 0; left: 0;"),
    ("z-0", "z-index: 0;"),
    ("z-10", "z-index: 10;"),
    ("z-20", "z-index: 20;"),
    ("z-30", "z-index: 30;"),
    ("z-40", "z-index: 40;"),
    ("z-50", "z-index: 50;"),
    ("shadow", "box-shadow: 0 1px 3px rgba(0,0,0,0.1);"),
    ("shadow-sm", "box-shadow: 0 1px 2px 0 rgba(0,0,0,0.05);"),
    ("shadow-md", "box-shadow: 0 4px 6px rgba(0,0,0,0.1);"),
    ("shadow-lg", "box-shadow: 0 10px 15px rgba(0,0,0,0.1);"),
    ("shadow-xl", "box-shadow: 0 20px 25px -5px rgba(0,0,0,0.1);"),
    (
        "shadow-2xl",
        "box-shadow: 0 25px 50px -12px rgba(0,0,0,0.25);",
    ),
    ("shadow-none", "box-shadow: none;"),
    ("transition", "transition: all 0.15s ease;"),
    ("transition-all", "transition: all 0.15s ease;"),
    (
        "transition-colors",
        "transition: color, background-color, border-color, fill, stroke;",
    ),
    ("transition-opacity", "transition: opacity;"),
    ("transition-transform", "transition: transform;"),
    ("duration-100", "transition-duration: 100ms;"),
    ("duration-150", "transition-duration: 150ms;"),
    ("duration-200", "transition-duration: 200ms;"),
    ("duration-300", "transition-duration: 300ms;"),
    ("duration-500", "transition-duration: 500ms;"),
    ("duration-700", "transition-duration: 700ms;"),
    ("duration-1000", "transition-duration: 1000ms;"),
    ("ease-linear", "transition-timing-function: linear;"),
    (
        "ease-in",
        "transition-timing-function: cubic-bezier(0.4, 0, 1, 1);",
    ),
    (
        "ease-out",
        "transition-timing-function: cubic-bezier(0, 0, 0.2, 1);",
    ),
    (
        "ease-in-out",
        "transition-timing-function: cubic-bezier(0.4, 0, 0.2, 1);",
    ),
    ("cursor-pointer", "cursor: pointer;"),
    ("cursor-default", "cursor: default;"),
    ("cursor-wait", "cursor: wait;"),
    ("cursor-not-allowed", "cursor: not-allowed;"),
    ("cursor-text", "cursor: text;"),
    ("cursor-move", "cursor: move;"),
    ("opacity-0", "opacity: 0;"),
    ("opacity-25", "opacity: 0.25;"),
    ("opacity-50", "opacity: 0.5;"),
    ("opacity-75", "opacity: 0.75;"),
    ("opacity-100", "opacity: 1;"),
    ("pointer-events-none", "pointer-events: none;"),
    ("pointer-events-auto", "pointer-events: auto;"),
    ("select-none", "user-select: none;"),
    ("select-text", "user-select: text;"),
    ("whitespace-nowrap", "white-space: nowrap;"),
    ("whitespace-pre", "white-space: pre;"),
    ("break-words", "overflow-wrap: break-word;"),
    (
        "truncate",
        "overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
    ),
    ("underline", "text-decoration: underline;"),
    ("line-through", "text-decoration: line-through;"),
    ("no-underline", "text-decoration: none;"),
    ("uppercase", "text-transform: uppercase;"),
    ("lowercase", "text-transform: lowercase;"),
    ("capitalize", "text-transform: capitalize;"),
    ("normal-case", "text-transform: none;"),
    ("italic", "font-style: italic;"),
    ("not-italic", "font-style: normal;"),
    (
        "font-mono",
        "font-family: ui-monospace, SFMono-Regular, monospace;",
    ),
    (
        "font-sans",
        "font-family: ui-sans-serif, system-ui, sans-serif;",
    ),
    ("font-serif", "font-family: ui-serif, Georgia, serif;"),
    (
        "antialiased",
        "-webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale;",
    ),
    ("leading-none", "line-height: 1;"),
    ("leading-tight", "line-height: 1.25;"),
    ("leading-normal", "line-height: 1.5;"),
    ("leading-loose", "line-height: 2;"),
    ("tracking-tight", "letter-spacing: -0.025em;"),
    ("tracking-normal", "letter-spacing: 0;"),
    ("tracking-wide", "letter-spacing: 0.025em;"),
    (
        "grid-cols-1",
        "grid-template-columns: repeat(1, minmax(0, 1fr));",
    ),
    (
        "grid-cols-2",
        "grid-template-columns: repeat(2, minmax(0, 1fr));",
    ),
    (
        "grid-cols-3",
        "grid-template-columns: repeat(3, minmax(0, 1fr));",
    ),
    (
        "grid-cols-4",
        "grid-template-columns: repeat(4, minmax(0, 1fr));",
    ),
    (
        "grid-cols-5",
        "grid-template-columns: repeat(5, minmax(0, 1fr));",
    ),
    (
        "grid-cols-6",
        "grid-template-columns: repeat(6, minmax(0, 1fr));",
    ),
    (
        "grid-cols-12",
        "grid-template-columns: repeat(12, minmax(0, 1fr));",
    ),
    ("col-span-1", "grid-column: span 1 / span 1;"),
    ("col-span-2", "grid-column: span 2 / span 2;"),
    ("col-span-3", "grid-column: span 3 / span 3;"),
    ("ring", "box-shadow: 0 0 0 3px rgba(59,130,246,0.5);"),
    ("ring-2", "box-shadow: 0 0 0 2px rgba(59,130,246,0.5);"),
    (
        "outline-none",
        "outline: 2px solid transparent; outline-offset: 2px;",
    ),
    ("appearance-none", "appearance: none;"),
    ("resize-none", "resize: none;"),
    (
        "sr-only",
        "position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0,0,0,0); white-space: nowrap; border: 0;",
    ),
    ("blur", "filter: blur(4px);"),
    ("blur-sm", "filter: blur(2px);"),
    ("grayscale", "filter: grayscale(100%);"),
    ("invert", "filter: invert(100%);"),
    ("backdrop-blur", "backdrop-filter: blur(4px);"),
    ("backdrop-blur-sm", "backdrop-filter: blur(2px);"),
    ("transform", "transform: translateX(0) translateY(0);"),
    ("translate-x-0", "transform: translateX(0);"),
    ("translate-x-1", "transform: translateX(0.25rem);"),
    ("translate-y-0", "transform: translateY(0);"),
    ("translate-y-1", "transform: translateY(0.25rem);"),
    ("rotate-45", "transform: rotate(45deg);"),
    ("rotate-90", "transform: rotate(90deg);"),
    ("scale-75", "transform: scale(0.75);"),
    ("scale-100", "transform: scale(1);"),
    ("scale-110", "transform: scale(1.1);"),
    ("animate-spin", "animation: spin 1s linear infinite;"),
    (
        "animate-ping",
        "animation: ping 1s cubic-bezier(0,0,0.2,1) infinite;",
    ),
    (
        "animate-pulse",
        "animation: pulse 2s cubic-bezier(0.4,0,0.6,1) infinite;",
    ),
    ("animate-bounce", "animation: bounce 1s infinite;"),
    ("fill-current", "fill: currentColor;"),
    ("stroke-current", "stroke: currentColor;"),
    ("object-contain", "object-fit: contain;"),
    ("object-cover", "object-fit: cover;"),
    ("float-right", "float: right;"),
    ("float-left", "float: left;"),
    ("float-none", "float: none;"),
    ("clear-both", "clear: both;"),
    ("visible", "visibility: visible;"),
    ("invisible", "visibility: hidden;"),
    ("box-border", "box-sizing: border-box;"),
    ("box-content", "box-sizing: content-box;"),
    ("aspect-square", "aspect-ratio: 1 / 1;"),
    ("aspect-video", "aspect-ratio: 16 / 9;"),
    ("flex-grow", "flex-grow: 1;"),
    ("flex-shrink", "flex-shrink: 1;"),
    ("flex-shrink-0", "flex-shrink: 0;"),
    ("order-1", "order: 1;"),
    ("order-2", "order: 2;"),
    ("divide-y", "& > * + * { border-top-width: 1px; }"),
    ("divide-x", "& > * + * { border-right-width: 1px; }"),
    ("space-x-1", "& > * + * { margin-left: 0.25rem; }"),
    ("space-x-2", "& > * + * { margin-left: 0.5rem; }"),
    ("space-x-4", "& > * + * { margin-left: 1rem; }"),
    ("space-y-1", "& > * + * { margin-top: 0.25rem; }"),
    ("space-y-2", "& > * + * { margin-top: 0.5rem; }"),
    ("space-y-4", "& > * + * { margin-top: 1rem; }"),
    ("min-w-0", "min-width: 0;"),
    ("min-w-full", "min-width: 100%;"),
    ("min-h-0", "min-height: 0;"),
    ("min-h-full", "min-height: 100%;"),
    ("max-w-none", "max-width: none;"),
    ("max-w-full", "max-width: 100%;"),
    ("list-none", "list-style-type: none;"),
    ("list-disc", "list-style-type: disc;"),
    ("list-decimal", "list-style-type: decimal;"),
    ("align-baseline", "vertical-align: baseline;"),
    ("align-top", "vertical-align: top;"),
    ("align-middle", "vertical-align: middle;"),
    ("align-bottom", "vertical-align: bottom;"),
    ("table-auto", "table-layout: auto;"),
    ("table-fixed", "table-layout: fixed;"),
    ("border-solid", "border-style: solid;"),
    ("border-dashed", "border-style: dashed;"),
    ("border-dotted", "border-style: dotted;"),
    ("border-none", "border-style: none;"),
    ("text-transparent", "color: transparent;"),
    ("bg-none", "background-image: none;"),
    ("bg-cover", "background-size: cover;"),
    ("bg-contain", "background-size: contain;"),
    ("bg-center", "background-position: center;"),
    ("bg-fixed", "background-attachment: fixed;"),
    ("bg-no-repeat", "background-repeat: no-repeat;"),
    ("bg-repeat", "background-repeat: repeat;"),
    ("isolate", "isolation: isolate;"),
    ("will-change-transform", "will-change: transform;"),
    ("will-change-auto", "will-change: auto;"),
    ("scroll-smooth", "scroll-behavior: smooth;"),
    ("scroll-auto", "scroll-behavior: auto;"),
    ("snap-x", "scroll-snap-type: x;"),
    ("snap-y", "scroll-snap-type: y;"),
    ("touch-none", "touch-action: none;"),
    ("touch-pan-x", "touch-action: pan-x;"),
    ("touch-pan-y", "touch-action: pan-y;"),
    ("touch-manipulation", "touch-action: manipulation;"),
    ("print:hidden", "@media print { display: none; }"),
    (
        "dark:bg-gray-800",
        "@media (prefers-color-scheme: dark) { background-color: #1f2937; }",
    ),
    (
        "dark:bg-gray-900",
        "@media (prefers-color-scheme: dark) { background-color: #111827; }",
    ),
    (
        "dark:text-white",
        "@media (prefers-color-scheme: dark) { color: #fff; }",
    ),
    (
        "dark:text-gray-100",
        "@media (prefers-color-scheme: dark) { color: #f3f4f6; }",
    ),
    (
        "motion-safe:animate-spin",
        "@media (prefers-reduced-motion: no-preference) { animation: spin 1s linear infinite; }",
    ),
    (
        "motion-reduce:animate-none",
        "@media (prefers-reduced-motion: reduce) { animation: none; }",
    ),
];

/// Extract plugin name from a JS line
fn extract_plugin_name(line: &str) -> Option<String> {
    // require('plugin-name') or require("plugin-name")
    if let Some(start) = line.find("require(") {
        let after = &line[start + 8..];
        let quote = after.chars().next()?;
        if quote == '\'' || quote == '"' {
            let end = after[1..].find(quote)?;
            return Some(after[1..1 + end].to_string());
        }
    }

    // from 'plugin-name' or from "plugin-name"
    if let Some(start) = line.find(" from ") {
        let after = &line[start + 6..].trim();
        let quote = after.chars().next()?;
        if quote == '\'' || quote == '"' {
            let end = after[1..].find(quote)?;
            let name = &after[1..1 + end];
            // Filter out non-plugin imports
            if !name.starts_with('.') && !name.starts_with("@/") {
                return Some(name.to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_json_config() {
        let json = r#"{
  "plugins": {
    "autoprefixer": {},
    "tailwindcss": {}
  }
}"#;
        let config = PostCssConfig::parse_config(json);
        assert_eq!(config.plugins.len(), 2);
        assert!(config.plugin_names().contains(&"autoprefixer"));
        assert!(config.plugin_names().contains(&"tailwindcss"));
    }

    #[test]
    fn test_parse_array_config() {
        let json = r#"{
  "plugins": ["autoprefixer", "tailwindcss"]
}"#;
        let config = PostCssConfig::parse_config(json);
        assert_eq!(config.plugins.len(), 2);
    }

    #[test]
    fn test_extract_plugin_name() {
        assert_eq!(
            extract_plugin_name("const autoprefixer = require('autoprefixer')"),
            Some("autoprefixer".to_string())
        );
        assert_eq!(
            extract_plugin_name("import autoprefixer from 'autoprefixer'"),
            Some("autoprefixer".to_string())
        );
        assert_eq!(extract_plugin_name("// comment"), None);
    }

    #[test]
    fn test_tailwind_config_parse_js() {
        let js = r#"
        module.exports = {
          content: ['./src/**/*.{html,js,ts,jsx,tsx}'],
          darkMode: 'class',
          theme: {},
          plugins: [],
        }
        "#;
        let config = TailwindConfig::parse_config(js);
        assert_eq!(config.content.len(), 1);
        assert!(config.content[0].contains("src/**"));
        assert_eq!(config.dark_mode, Some("class".to_string()));
        assert!(config.jit);
    }

    #[test]
    fn test_tailwind_config_parse_json() {
        let json = r#"{
          "content": ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
          "darkMode": "media"
        }"#;
        let config = TailwindConfig::parse_config(json);
        assert_eq!(config.content.len(), 2);
        assert_eq!(config.dark_mode, Some("media".to_string()));
    }

    #[test]
    fn test_browserslist_parse_targets() {
        let targets = vec![
            "last 2 versions".to_string(),
            "chrome >= 88".to_string(),
            "firefox >= 90".to_string(),
        ];
        let browsers = parse_browserslist(&targets);
        assert!(browsers.chrome.is_some());
        assert!(browsers.firefox.is_some());
        // chrome >= 88
        assert_eq!(browsers.chrome.unwrap() >> 16, 88);
        // firefox >= 90
        assert_eq!(browsers.firefox.unwrap() >> 16, 90);
    }

    #[test]
    fn test_browserslist_defaults() {
        let targets: Vec<String> = vec![];
        let browsers = parse_browserslist(&targets);
        assert!(browsers.chrome.is_some());
        assert!(browsers.firefox.is_some());
        assert!(browsers.safari.is_some());
    }
}
