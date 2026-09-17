// Advanced CSS features: #66 composes, #67 dark mode, #68 custom property
// optimization, #69 scoped CSS for React, #70 nesting polyfill verification.

use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use tracing::{info, warn};

// ── Feature 66: CSS Modules composes ──────────────────────────────────

/// Parsed `composes` directive: `composes: button from './buttons.css'`
/// or `composes: button` (local composition)
#[derive(Debug, Clone)]
pub struct ComposesDirective {
    /// Local class name being composed into
    pub local_class: String,
    /// Class names to compose from
    pub source_classes: Vec<String>,
    /// Source file path (None = local composition)
    pub from_file: Option<String>,
}

/// Parse `composes` directives from CSS source.
/// Handles both local (`composes: button`) and cross-file (`composes: button from './btn.css'`).
pub fn parse_composes(css: &str, _file_path: &str) -> Vec<ComposesDirective> {
    static COMPOSES_RE: OnceLock<Regex> = OnceLock::new();
    let re = COMPOSES_RE
        .get_or_init(|| Regex::new(r"\.([a-zA-Z_][\w-]*)\s*\{[^}]*composes:\s*([^;]+);").unwrap());

    let mut directives = Vec::new();

    for cap in re.captures_iter(css) {
        let local_class = cap[1].to_string();
        let raw = cap[2].trim();

        // Check for "from" keyword: `button from './buttons.css'`
        if let Some(from_pos) = raw.find(" from ") {
            let classes_str = &raw[..from_pos].trim();
            let from_file = raw[from_pos + 6..]
                .trim()
                .trim_matches(|c| c == '"' || c == '\'');

            let source_classes: Vec<String> = classes_str
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();

            directives.push(ComposesDirective {
                local_class,
                source_classes,
                from_file: Some(from_file.to_string()),
            });
        } else {
            // Local composition
            let source_classes: Vec<String> =
                raw.split_whitespace().map(|s| s.to_string()).collect();

            directives.push(ComposesDirective {
                local_class,
                source_classes,
                from_file: None,
            });
        }
    }

    directives
}

/// Resolve composes directives by loading source files and merging class mappings.
/// Returns a map of local class → composed class list (scoped names).
pub fn resolve_composes(
    css: &str,
    file_path: &str,
    css_module_map: &[(String, String)],
    _root: &Path,
) -> HashMap<String, Vec<String>> {
    let directives = parse_composes(css, file_path);
    let mut result = HashMap::new();

    // Build a lookup from original → scoped name
    let local_map: HashMap<&str, &str> = css_module_map
        .iter()
        .map(|(o, s)| (o.as_str(), s.as_str()))
        .collect();

    for dir in &directives {
        let mut composed_classes = Vec::new();

        // Add the local scoped class
        if let Some(scoped) = local_map.get(dir.local_class.as_str()) {
            composed_classes.push(scoped.to_string());
        }

        // Add composed classes
        for src_class in &dir.source_classes {
            if let Some(ref from_file) = dir.from_file {
                // Cross-file: load the source file's CSS module map
                let source_path = Path::new(file_path)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(from_file);

                if source_path.is_file()
                    && let Ok(source_css) = std::fs::read_to_string(&source_path)
                {
                    let source_map =
                        generate_css_module_map(&source_css, &source_path.to_string_lossy());
                    for (orig, scoped) in &source_map {
                        if orig == src_class {
                            composed_classes.push(scoped.clone());
                        }
                    }
                }
            } else {
                // Local composition
                if let Some(scoped) = local_map.get(src_class.as_str()) {
                    composed_classes.push(scoped.to_string());
                }
            }
        }

        result.insert(dir.local_class.clone(), composed_classes);
    }

    result
}

/// Remove `composes:` directives from CSS after resolution.
pub fn strip_composes(css: &str) -> String {
    static COMPOSES_LINE_RE: OnceLock<Regex> = OnceLock::new();
    let re = COMPOSES_LINE_RE.get_or_init(|| Regex::new(r"^\s*composes:\s*[^;]+;\s*$").unwrap());

    css.lines()
        .filter(|line| !re.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Feature 67: Dark mode CSS generation ──────────────────────────────

/// Generate dark mode CSS variants from a stylesheet.
/// Strategy: detect `prefers-color-scheme: dark` media queries and extract
/// them, or auto-generate dark variants using CSS custom property inversion.
pub fn generate_dark_mode_css(css: &str, strategy: &str) -> String {
    match strategy {
        "auto" => auto_dark_mode(css),
        "extract" => extract_dark_media(css),
        _ => css.to_string(),
    }
}

/// Auto-generate dark mode by inverting lightness of color values via CSS custom properties.
/// Handles both `:root` globals and component-scoped custom properties — every
/// rule that defines `--var: <color>` gets a dark variant under the same selector.
fn auto_dark_mode(css: &str) -> String {
    // Check if the CSS already has prefers-color-scheme
    if css.contains("prefers-color-scheme: dark") {
        return css.to_string();
    }

    // Match every rule whose body declares at least one custom property.
    // The selector group excludes `@` so at-rule preludes are not captured
    // (they're also filtered below by checking the preceding character).
    static RULE_VAR_RE: OnceLock<Regex> = OnceLock::new();
    let re = RULE_VAR_RE
        .get_or_init(|| Regex::new(r"([^{}@]+)\{([^{}]*--[a-zA-Z_][\w-]*\s*:[^}]*)\}").unwrap());

    let mut blocks: Vec<(String, Vec<String>)> = Vec::new();

    for cap in re.captures_iter(css) {
        let m = cap.get(0).unwrap();
        // Skip at-rule preludes like `@media ... {` — the regex can match the
        // text after `@` since `@` itself is excluded from the selector group.
        if m.start() > 0 && css.as_bytes()[m.start() - 1] == b'@' {
            continue;
        }

        let selector = cap[1].trim();
        let body = &cap[2];
        let mut dark_vars = Vec::new();

        // Parse custom property declarations (split on ';' so a trailing
        // declaration without a semicolon is still handled)
        for decl in body.split(';') {
            let trimmed = decl.trim();
            if trimmed.starts_with("--")
                && let Some(colon_pos) = trimmed.find(':')
            {
                let name = trimmed[..colon_pos].trim();
                let value = trimmed[colon_pos + 1..].trim();

                // Generate dark variant by checking for color-like values
                if is_color_value(value) {
                    let dark_value = invert_color_lightness(value);
                    dark_vars.push(format!("    {}: {};", name, dark_value));
                }
            }
        }

        if !dark_vars.is_empty() {
            blocks.push((selector.to_string(), dark_vars));
        }
    }

    if !blocks.is_empty() {
        let mut dark_block = String::from("\n\n@media (prefers-color-scheme: dark) {");
        for (selector, vars) in &blocks {
            dark_block.push_str(&format!("\n  {} {{\n{}\n  }}", selector, vars.join("\n")));
        }
        dark_block.push_str("\n}");
        info!(
            "Generated dark mode custom properties for {} selector(s)",
            blocks.len()
        );
        return format!("{}{}", css, dark_block);
    }

    warn!("No CSS custom properties found for dark mode generation");
    css.to_string()
}

/// Extract dark mode media queries into a separate block.
fn extract_dark_media(css: &str) -> String {
    static MEDIA_RE: OnceLock<Regex> = OnceLock::new();
    let re = MEDIA_RE.get_or_init(|| {
        Regex::new(r"@media\s*\(prefers-color-scheme:\s*dark\)\s*\{([^}]*(?:\{[^}]*\}[^}]*)*)\}")
            .unwrap()
    });

    let mut dark_css = String::new();
    for cap in re.captures_iter(css) {
        dark_css.push_str(&cap[0]);
        dark_css.push('\n');
    }

    if dark_css.is_empty() {
        css.to_string()
    } else {
        format!("{}\n\n{}", css, dark_css)
    }
}

fn is_color_value(value: &str) -> bool {
    let v = value.trim().to_lowercase();
    v.starts_with('#')
        || v.starts_with("rgb(")
        || v.starts_with("rgba(")
        || v.starts_with("hsl(")
        || v.starts_with("hsla(")
        || v.starts_with("color(")
}

/// Invert lightness of a color value for dark mode.
///
/// Converts the color to HSL, inverts only the lightness channel (l → 1 - l),
/// and converts back. This preserves hue and saturation, so `#ff0000` stays red
/// instead of becoming `#00ffff` as naive per-channel RGB inversion would do.
fn invert_color_lightness(value: &str) -> String {
    let v = value.trim();

    // Handle hex colors: #rgb, #rgba, #rrggbb, #rrggbbaa
    if let Some(hex) = v.strip_prefix('#') {
        let expanded: String = match hex.len() {
            3 | 4 => hex.chars().flat_map(|c| [c, c]).collect(),
            _ => hex.to_string(),
        };
        if (expanded.len() == 6 || expanded.len() == 8)
            && let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&expanded[0..2], 16),
                u8::from_str_radix(&expanded[2..4], 16),
                u8::from_str_radix(&expanded[4..6], 16),
            )
        {
            let (nr, ng, nb) = invert_rgb_lightness(r, g, b);
            if expanded.len() == 8 {
                return format!("#{:02x}{:02x}{:02x}{}", nr, ng, nb, &expanded[6..8]);
            }
            return format!("#{:02x}{:02x}{:02x}", nr, ng, nb);
        }
        return v.to_string();
    }

    // Handle rgb()/rgba() — comma or space separated, optional alpha via , or /
    if v.starts_with("rgb(") || v.starts_with("rgba(") {
        static RGB_RE: OnceLock<Regex> = OnceLock::new();
        let re = RGB_RE.get_or_init(|| {
            Regex::new(
                r"rgba?\(\s*(\d+)\s*[,\s]\s*(\d+)\s*[,\s]\s*(\d+)\s*(?:[,/]\s*([\d.]+%?)\s*)?\)",
            )
            .unwrap()
        });
        if let Some(cap) = re.captures(v) {
            let r: u8 = cap[1].parse().unwrap_or(0);
            let g: u8 = cap[2].parse().unwrap_or(0);
            let b: u8 = cap[3].parse().unwrap_or(0);
            let (nr, ng, nb) = invert_rgb_lightness(r, g, b);
            if let Some(alpha) = cap.get(4) {
                return format!("rgba({}, {}, {}, {})", nr, ng, nb, alpha.as_str());
            }
            return format!("rgb({}, {}, {})", nr, ng, nb);
        }
    }

    // Handle hsl()/hsla() — invert the lightness channel directly
    if v.starts_with("hsl(") || v.starts_with("hsla(") {
        static HSL_RE: OnceLock<Regex> = OnceLock::new();
        let re = HSL_RE.get_or_init(|| {
            Regex::new(
                r"hsla?\(\s*([\d.]+)\s*[,\s]\s*([\d.]+)%\s*[,\s]\s*([\d.]+)%\s*(?:[,/]\s*([\d.]+%?)\s*)?\)",
            )
            .unwrap()
        });
        if let Some(cap) = re.captures(v) {
            let h: f64 = cap[1].parse().unwrap_or(0.0);
            let s: f64 = cap[2].parse().unwrap_or(0.0);
            let l: f64 = cap[3].parse().unwrap_or(0.0);
            let dark_l = (100.0 - l).clamp(0.0, 100.0);
            if let Some(alpha) = cap.get(4) {
                return format!("hsla({}, {}%, {}%, {})", h, s, dark_l, alpha.as_str());
            }
            return format!("hsl({}, {}%, {}%)", h, s, dark_l);
        }
    }

    // Can't invert — return as-is
    v.to_string()
}

/// Invert only the lightness channel of an RGB color, preserving hue/saturation.
fn invert_rgb_lightness(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsl_to_rgb(h, s, 1.0 - l)
}

/// Convert RGB (0–255) to HSL (h: 0–360, s/l: 0–1).
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let r = r as f64 / 255.0;
    let g = g as f64 / 255.0;
    let b = b as f64 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < f64::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h * 60.0, s, l)
}

/// Convert HSL (h: 0–360, s/l: 0–1) to RGB (0–255).
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let h = (((h % 360.0) + 360.0) % 360.0) / 360.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    if s < f64::EPSILON {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |t: f64| -> u8 {
        let mut t = t;
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        let c = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (c * 255.0).round().clamp(0.0, 255.0) as u8
    };
    (channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0))
}

// ── Feature 68: CSS custom properties optimization ────────────────────

/// Optimize CSS custom properties:
/// 1. Inline static custom properties (used in only one place)
/// 2. Remove unused :root variables
/// 3. Minify custom property names in production
pub fn optimize_custom_properties(css: &str, minify_names: bool) -> String {
    let mut result = css.to_string();

    // Step 1: Extract all custom property definitions
    let props = extract_custom_properties(&result);

    if props.is_empty() {
        return result;
    }

    // Step 2: Find usage count for each property
    let usage_counts = count_property_usage(&result, &props);

    // Step 3: Remove unused properties
    let mut removed = 0;
    for (name, _) in &props {
        if usage_counts.get(name.as_str()).copied().unwrap_or(0) == 0 {
            // Remove the property definition
            let pattern = format!(r"\s*{}\s*:\s*[^;]+;\s*", regex::escape(name));
            if let Ok(re) = Regex::new(&pattern) {
                result = re.replace(&result, "").to_string();
                removed += 1;
            }
        }
    }

    if removed > 0 {
        info!("Removed {} unused CSS custom properties", removed);
    }

    // Step 4: Inline single-use properties
    let mut inlined = 0;
    for (name, value) in &props {
        if usage_counts.get(name.as_str()).copied().unwrap_or(0) == 1 {
            // Replace var(name) with the value
            let pattern = format!(r"var\(\s*{}\s*\)", regex::escape(name));
            if let Ok(re) = Regex::new(&pattern) {
                let before = result.clone();
                result = re.replace(&result, value.as_str()).to_string();
                if result != before {
                    inlined += 1;
                }
            }
            // Remove the definition
            let def_pattern = format!(r"\s*{}\s*:\s*[^;]+;\s*", regex::escape(name));
            if let Ok(re) = Regex::new(&def_pattern) {
                result = re.replace(&result, "").to_string();
            }
        }
    }

    if inlined > 0 {
        info!("Inlined {} single-use CSS custom properties", inlined);
    }

    // Step 5: Minify property names in production
    if minify_names {
        result = minify_property_names(&result, &props);
    }

    result
}

fn extract_custom_properties(css: &str) -> Vec<(String, String)> {
    static PROP_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        PROP_RE.get_or_init(|| Regex::new(r"(?:^|\s)(--[a-zA-Z_][\w-]*)\s*:\s*([^;]+);").unwrap());

    re.captures_iter(css)
        .map(|cap| (cap[1].to_string(), cap[2].trim().to_string()))
        .collect()
}

fn count_property_usage(css: &str, props: &[(String, String)]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for (name, _) in props {
        let pattern = format!(r"var\(\s*{}\s*\)", regex::escape(name));
        if let Ok(re) = Regex::new(&pattern) {
            let count = re.find_iter(css).count();
            counts.insert(name.clone(), count);
        }
    }
    counts
}

fn minify_property_names(css: &str, props: &[(String, String)]) -> String {
    let mut result = css.to_string();
    let mut name_map = HashMap::new();

    for (i, (name, _)) in props.iter().enumerate() {
        // Generate short name: --a, --b, --c, ...
        let short_name = format!("--{}", char::from_u32('a' as u32 + i as u32).unwrap_or('z'));
        name_map.insert(name.clone(), short_name);
    }

    // Replace definitions
    for (name, short) in &name_map {
        let pattern = regex::escape(name);
        if let Ok(re) = Regex::new(&pattern) {
            result = re.replace_all(&result, short.as_str()).to_string();
        }
    }

    result
}

// ── Feature 69: Scoped CSS for React ──────────────────────────────────

/// Generate a scoped attribute hash for a CSS file (like Vue's data-v-xxxxx).
pub fn generate_scope_hash(file_path: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_path.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Scope CSS selectors with a data attribute: `.button` → `.button[data-v-abc123]`
pub fn scope_css_with_attribute(css: &str, scope_hash: &str) -> String {
    let attr = format!("[data-v-{}]", scope_hash);

    // Scope all class selectors: .classname followed by delimiter or end
    static SELECTOR_RE: OnceLock<Regex> = OnceLock::new();
    let re = SELECTOR_RE.get_or_init(|| Regex::new(r"(\.[a-zA-Z_][\w-]*)").unwrap());

    let result = re.replace_all(css, |caps: &regex::Captures| {
        format!("{}{}", &caps[1], attr)
    });

    result.to_string()
}

/// Inject scope attribute into a React component's JSX.
/// Adds `data-v-xxxxx` to the root element of the component.
pub fn inject_scope_attribute(code: &str, scope_hash: &str) -> String {
    let attr = format!("data-v-{}", scope_hash);

    // Find the first JSX opening tag and inject the attribute
    static JSX_TAG_RE: OnceLock<Regex> = OnceLock::new();
    let re = JSX_TAG_RE.get_or_init(|| Regex::new(r"(<[a-zA-Z][\w.-]*(?:\s[^>]*)?)(>)").unwrap());

    if let Some(m) = re.captures(code) {
        let before = &code[..m[1].len()];
        let after = &code[m[1].len()..];
        // Check if attribute already exists
        if !before.contains(&attr) {
            return format!("{} {}{}", before, attr, after);
        }
    }

    code.to_string()
}

// ── Feature 70: CSS nesting polyfill ──────────────────────────────────

/// Check if CSS contains native nesting syntax.
pub fn has_native_nesting(css: &str) -> bool {
    // Detect & selector (nesting parent reference)
    // Matches & anywhere in the CSS (not just at line start)
    static NESTING_RE: OnceLock<Regex> = OnceLock::new();
    let re = NESTING_RE.get_or_init(|| Regex::new(r"&[\s.{:>+~(]").unwrap());
    if re.is_match(css) {
        return true;
    }

    // Structural check: a style rule whose body contains another rule block
    // is nested CSS (e.g. `.a { .b {} }` or `.a { @media ... {} }`).
    fn body_has_block(body: &str) -> bool {
        parse_css_items(body)
            .iter()
            .any(|item| matches!(item, CssItem::Block(..)))
    }

    for item in parse_css_items(css) {
        if let CssItem::Block(prelude, body) = &item {
            if prelude.trim().starts_with('@') {
                // Inside at-rules, look for style rules that themselves nest
                for inner in parse_css_items(body) {
                    if let CssItem::Block(ip, ib) = inner
                        && !ip.trim().starts_with('@')
                        && body_has_block(&ib)
                    {
                        return true;
                    }
                }
            } else if body_has_block(body) {
                return true;
            }
        }
    }
    false
}

/// Polyfill CSS nesting for older browsers.
/// In production this is handled by lightningcss's minify pass; in dev mode
/// (no minify) this function flattens `&` nested rules explicitly so the
/// emitted CSS works in browsers without native nesting support.
pub fn polyfill_nesting(css: &str) -> String {
    if !has_native_nesting(css) {
        return css.to_string();
    }
    info!("CSS nesting detected — transpiling for dev mode");
    let flattened = flatten_nesting(css);
    if has_native_nesting(&flattened) {
        warn!("Some nested CSS could not be fully transpiled");
    }
    flattened
}

// ── Simple CSS nesting flattener ──────────────────────────────────────
//
// Expands `&` references in nested rules to the parent selector:
//   .parent { &:hover { color: blue } } → .parent { } .parent:hover { color: blue }
//
// This is intentionally simplified — it handles the common cases (`&` anywhere
// in a nested selector, descendant selectors, nested at-rules like @media).
// Comments inside processed blocks are dropped. Complex cases that survive
// are passed through in declaration position.

/// A top-level item in a CSS block: either a `;`-terminated statement
/// (declaration, @import, etc.) or a prelude + `{ ... }` block.
enum CssItem {
    Statement(String),
    Block(String, String),
}

/// Split a CSS fragment into statements and braced blocks, skipping over
/// string literals and comments when looking for `;`, `{`, `}` boundaries.
fn parse_css_items(css: &str) -> Vec<CssItem> {
    let chars: Vec<char> = css.chars().collect();
    let n = chars.len();
    let mut items = Vec::new();
    let mut i = 0;

    while i < n {
        // Skip leading whitespace between items
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Scan prelude text until a top-level `;`, `{`, or `}`
        let start = i;
        let mut boundary: Option<char> = None;
        while i < n {
            match chars[i] {
                '"' | '\'' => i = skip_css_string(&chars, i),
                '/' if i + 1 < n && chars[i + 1] == '*' => i = skip_css_comment(&chars, i),
                '{' | ';' | '}' => {
                    boundary = Some(chars[i]);
                    break;
                }
                _ => i += 1,
            }
        }
        let text: String = chars[start..i].iter().collect();

        match boundary {
            Some('{') => {
                // Find the matching close brace, skipping strings/comments
                let body_start = i + 1;
                let mut depth = 1;
                let mut j = body_start;
                while j < n && depth > 0 {
                    match chars[j] {
                        '"' | '\'' => j = skip_css_string(&chars, j),
                        '/' if j + 1 < n && chars[j + 1] == '*' => j = skip_css_comment(&chars, j),
                        '{' => {
                            depth += 1;
                            j += 1;
                        }
                        '}' => {
                            depth -= 1;
                            j += 1;
                        }
                        _ => j += 1,
                    }
                }
                let body_end = if depth == 0 { j - 1 } else { n };
                items.push(CssItem::Block(
                    text,
                    chars[body_start..body_end].iter().collect(),
                ));
                i = j;
            }
            Some(';') => {
                items.push(CssItem::Statement(format!("{};", text)));
                i += 1;
            }
            _ => {
                // Trailing text (or a stray '}') — emit as a statement
                if !text.trim().is_empty() {
                    items.push(CssItem::Statement(text));
                }
                i += 1;
            }
        }
    }

    items
}

/// Advance past a string literal starting at `chars[start]` (a quote char).
/// Returns the index just after the closing quote.
fn skip_css_string(chars: &[char], start: usize) -> usize {
    let quote = chars[start];
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
            continue;
        }
        if chars[i] == quote {
            return i + 1;
        }
        i += 1;
    }
    i
}

/// Advance past a `/* ... */` comment starting at `start` (the `/`).
/// Returns the index just after `*/`.
fn skip_css_comment(chars: &[char], start: usize) -> usize {
    let mut i = start + 2;
    while i + 1 < chars.len() {
        if chars[i] == '*' && chars[i + 1] == '/' {
            return i + 2;
        }
        i += 1;
    }
    chars.len()
}

/// Flatten a stylesheet fragment, expanding nested `&` rules.
fn flatten_nesting(css: &str) -> String {
    let mut out = String::with_capacity(css.len());
    emit_top_level(css, &mut out);
    out
}

/// Emit top-level CSS items: statements pass through, at-rules keep their
//  wrapper with contents flattened recursively, style rules are flattened
//  via `emit_rule`.
fn emit_top_level(css: &str, out: &mut String) {
    for item in parse_css_items(css) {
        match item {
            CssItem::Statement(s) => {
                out.push_str(&s);
                out.push('\n');
            }
            CssItem::Block(prelude, body) => {
                let p = prelude.trim();
                if p.starts_with('@') {
                    // At-rule: contents are standalone rules — recurse at top level
                    out.push_str(p);
                    out.push_str(" {");
                    emit_top_level(&body, out);
                    out.push_str("}\n");
                } else {
                    emit_rule(p, &body, out);
                }
            }
        }
    }
}

/// Emit a style rule: `selector { decls }` followed by each nested rule
/// resolved against `selector` and emitted as a sibling top-level rule.
fn emit_rule(selector: &str, body: &str, out: &mut String) {
    let mut decls = String::new();
    let mut nested: Vec<(String, String)> = Vec::new();

    for item in parse_css_items(body) {
        match item {
            CssItem::Statement(s) => decls.push_str(&s),
            CssItem::Block(p, b) => nested.push((p, b)),
        }
    }

    // Emit the parent's own declarations (skip an empty rule when the body
    // consisted solely of nested rules)
    if !decls.trim().is_empty() || nested.is_empty() {
        out.push_str(selector);
        out.push_str(" {");
        out.push_str(&decls);
        out.push_str("}\n");
    }

    for (prelude, nbody) in &nested {
        let p = prelude.trim();
        if p.starts_with('@') {
            // Nested at-rule (@media, @supports): keep the wrapper and resolve
            // its contents against the enclosing selector
            out.push_str(p);
            out.push_str(" {");
            emit_rule(selector, nbody, out);
            out.push_str("}\n");
        } else {
            emit_rule(&resolve_nested_selector(p, selector), nbody, out);
        }
    }
}

/// Resolve a nested selector against its parent selector.
/// `&` is replaced by the parent; a nested selector without `&` is treated
/// as a descendant per the CSS nesting spec. Comma-separated parents are
/// wrapped in `:is()` so `&:hover` resolves correctly.
fn resolve_nested_selector(nested: &str, parent: &str) -> String {
    let parent = if parent.contains(',') {
        format!(":is({})", parent)
    } else {
        parent.to_string()
    };
    if nested.contains('&') {
        nested.replace('&', &parent)
    } else {
        format!("{} {}", parent, nested)
    }
}

// ── Helper: reuse CSS module map generation ───────────────────────────

/// Generate CSS module class name mappings (reused from transform.rs).
fn generate_css_module_map(css: &str, file_path: &str) -> Vec<(String, String)> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_path.hash(&mut hasher);
    let file_hash = format!("{:x}", hasher.finish());
    let short_hash = &file_hash[..6];

    static CLASS_RE: OnceLock<Regex> = OnceLock::new();
    let re = CLASS_RE.get_or_init(|| Regex::new(r"\.([a-zA-Z_][\w-]*)").unwrap());

    let mut mappings = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for cap in re.captures_iter(css) {
        let class = cap[1].to_string();
        if !seen.contains(&class) {
            seen.insert(class.clone());
            let scoped = format!("_{}_{}", class, short_hash);
            mappings.push((class, scoped));
        }
    }

    mappings
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_composes_local() {
        let css = ".btn { color: red; composes: base; }";
        let dirs = parse_composes(css, "test.css");
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].local_class, "btn");
        assert_eq!(dirs[0].source_classes, vec!["base"]);
        assert!(dirs[0].from_file.is_none());
    }

    #[test]
    fn test_parse_composes_cross_file() {
        let css = ".btn { composes: button from './buttons.css'; }";
        let dirs = parse_composes(css, "test.css");
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].local_class, "btn");
        assert_eq!(dirs[0].source_classes, vec!["button"]);
        assert_eq!(dirs[0].from_file.as_deref(), Some("./buttons.css"));
    }

    #[test]
    fn test_strip_composes() {
        let css = ".btn {\n  color: red;\n  composes: base;\n}";
        let result = strip_composes(css);
        assert!(!result.contains("composes"));
        assert!(result.contains("color: red"));
    }

    #[test]
    fn test_dark_mode_auto() {
        let css = ":root { --bg: #ffffff; --text: #000000; }";
        let result = generate_dark_mode_css(css, "auto");
        assert!(result.contains("prefers-color-scheme: dark"));
        assert!(result.contains("--bg"));
    }

    #[test]
    fn test_dark_mode_no_existing() {
        let css = ".btn { color: red; }";
        let result = generate_dark_mode_css(css, "auto");
        // No custom properties, should return as-is
        assert_eq!(result, css);
    }

    #[test]
    fn test_invert_hex_color() {
        assert_eq!(invert_color_lightness("#ffffff"), "#000000");
        assert_eq!(invert_color_lightness("#000000"), "#ffffff");
        // HSL lightness inversion preserves hue: pure red (l=0.5) stays red,
        // dark blue becomes light blue rather than yellow
        assert_eq!(invert_color_lightness("#ff0000"), "#ff0000");
        assert_eq!(invert_color_lightness("#003366"), "#99ccff");
    }

    #[test]
    fn test_invert_hsl() {
        assert_eq!(
            invert_color_lightness("hsl(210, 50%, 40%)"),
            "hsl(210, 50%, 60%)"
        );
    }

    #[test]
    fn test_invert_rgb() {
        assert_eq!(invert_color_lightness("rgb(255, 255, 255)"), "rgb(0, 0, 0)");
        assert_eq!(
            invert_color_lightness("rgba(0, 0, 0, 0.5)"),
            "rgba(255, 255, 255, 0.5)"
        );
    }

    #[test]
    fn test_optimize_custom_properties_remove_unused() {
        let css = ":root { --unused: red; --used: blue; } .btn { color: var(--used); }";
        let result = optimize_custom_properties(css, false);
        assert!(!result.contains("--unused"));
        assert!(result.contains("--used") || result.contains("blue"));
    }

    #[test]
    fn test_optimize_custom_properties_inline_single() {
        let css = ":root { --once: red; } .btn { color: var(--once); }";
        let result = optimize_custom_properties(css, false);
        assert!(result.contains("red"));
        assert!(!result.contains("--once"));
    }

    #[test]
    fn test_scope_css_with_attribute() {
        let css = ".btn { color: red; } .card { padding: 10px; }";
        let result = scope_css_with_attribute(css, "abc123");
        assert!(result.contains("[data-v-abc123]"));
    }

    #[test]
    fn test_generate_scope_hash() {
        let hash1 = generate_scope_hash("src/Button.css");
        let hash2 = generate_scope_hash("src/Button.css");
        let hash3 = generate_scope_hash("src/Card.css");
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_inject_scope_attribute() {
        let code = "function Button() { return <button>Click</button>; }";
        let result = inject_scope_attribute(code, "abc123");
        assert!(result.contains("data-v-abc123"));
    }

    #[test]
    fn test_has_native_nesting() {
        assert!(has_native_nesting(
            ".btn { color: red; &:hover { color: blue; } }"
        ));
        assert!(!has_native_nesting(".btn { color: red; }"));
    }

    #[test]
    fn test_dark_mode_component_scoped() {
        let css = ".card { --bg: #ffffff; --border: #cccccc; }";
        let result = generate_dark_mode_css(css, "auto");
        assert!(result.contains("prefers-color-scheme: dark"));
        assert!(result.contains(".card {"));
        assert!(result.contains("--bg: #000000"));
    }

    #[test]
    fn test_polyfill_nesting_hover() {
        let css = ".btn { color: red; &:hover { color: blue; } }";
        let result = polyfill_nesting(css);
        assert!(result.contains(".btn:hover"));
        assert!(!result.contains("&"));
        assert!(result.contains("color: red"));
    }

    #[test]
    fn test_polyfill_nesting_descendant() {
        // Descendant nesting has no `&` so polyfill_nesting's detection skips
        // it — test the flattener directly.
        let css = ".parent { .child { color: red; } }";
        let result = flatten_nesting(css);
        assert!(result.contains(".parent .child"));
    }

    #[test]
    fn test_polyfill_nesting_media() {
        let css = ".btn { color: red; @media (min-width: 100px) { &:hover { color: blue; } } }";
        let result = polyfill_nesting(css);
        assert!(result.contains("@media (min-width: 100px)"));
        assert!(result.contains(".btn:hover"));
    }

    #[test]
    fn test_polyfill_nesting_noop() {
        let css = ".btn { color: red; }";
        assert_eq!(polyfill_nesting(css), css);
    }
}
