// CSS transforms: Lightning CSS, CSS Modules, PostCSS/Tailwind, Sass

use super::TransformOutput;
use crate::config::PledgeConfig;
use anyhow::Result;

/// Transform CSS using Lightning CSS
/// - Minification (production)
/// - Nesting transpilation
/// - Autoprefixing (browser targets)
/// - CSS Modules (if file is *.module.css)
pub(super) fn transform_css(
    source: &str,
    file_path: &str,
    is_production: bool,
    config: &PledgeConfig,
) -> Result<TransformOutput> {
    use lightningcss::stylesheet::{ParserOptions, PrinterOptions, StyleSheet};

    let is_css_module = file_path.ends_with(".module.css");

    let tw_v4 = crate::tailwind_v4::TailwindV4Theme::from_css(source);
    let processed_source = if tw_v4.is_v4 {
        crate::tailwind_v4::process_tailwind_v4(source, &config.root)
    } else {
        let postcss_config = crate::postcss::PostCssConfig::from_file(&config.root);
        if let Some(ref pc) = postcss_config {
            crate::postcss::process_css(source, file_path, pc, &config.root, is_production)
        } else {
            process_postcss(source, file_path)
        }
    };

    // Resolve browser targets from browserslist (package.json or .browserslistrc),
    // falling back to modern-browser defaults. Targets drive autoprefixing and
    // lowering of modern CSS features (e.g. nesting) at print time — note that
    // ParserOptions has no `targets` field in lightningcss; targets are applied
    // via MinifyOptions and PrinterOptions instead.
    let browsers = crate::postcss::BrowserslistConfig::from_root(&config.root);
    let targets = lightningcss::targets::Targets {
        browsers: Some(browsers.browser_targets()),
        ..Default::default()
    };

    let mut stylesheet =
        StyleSheet::parse(&processed_source, ParserOptions::default()).map_err(|e| {
            crate::diagnostics::css_error(
                file_path,
                &processed_source,
                &config.root,
                &e.kind.to_string(),
                e.loc.as_ref().map(|l| (l.line, l.column)),
            )
        })?;

    if is_production {
        stylesheet
            .minify(lightningcss::stylesheet::MinifyOptions {
                targets,
                ..Default::default()
            })
            .map_err(|e| anyhow::anyhow!("CSS minify error in {}: {}", file_path, e))?;
    }

    let printer_options = PrinterOptions {
        minify: is_production,
        targets,
        ..Default::default()
    };

    let result = stylesheet
        .to_css(printer_options)
        .map_err(|e| anyhow::anyhow!("CSS serialize error in {}: {}", file_path, e))?;

    let css_code = if !is_production {
        // Dev mode: minify isn't run, so explicitly transpile native `&` nesting
        // (targets above already lower it at print time; this is a safety net)
        // and polyfill container queries.
        let polyfilled = crate::css_features::polyfill_container_queries(&result.code);
        crate::css_advanced::polyfill_nesting(&polyfilled)
    } else {
        result.code
    };

    let css_modules = if is_css_module {
        let css_module_map = generate_css_module_map(&css_code, file_path);
        Some(css_module_map)
    } else {
        None
    };

    let css_code = if config.css.dark_mode != "off" {
        crate::css_advanced::generate_dark_mode_css(&css_code, &config.css.dark_mode)
    } else {
        css_code
    };

    let css_code = if is_production && config.css.optimize_custom_properties {
        crate::css_advanced::optimize_custom_properties(
            &css_code,
            config.css.minify_custom_property_names,
        )
    } else {
        css_code
    };

    let css_code = if config.css.scoped == "attribute" {
        let scope_hash = crate::css_advanced::generate_scope_hash(file_path);
        crate::css_advanced::scope_css_with_attribute(&css_code, &scope_hash)
    } else {
        css_code
    };

    let css_code = if is_css_module {
        crate::css_advanced::strip_composes(&css_code)
    } else {
        css_code
    };

    let source_map = if config.source_maps {
        Some(crate::css_features::generate_css_source_map(
            file_path, source, &css_code,
        ))
    } else {
        None
    };

    Ok(TransformOutput {
        code: css_code,
        source_map,
        css_modules,
        is_css: true,
        extracted_css: None,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}

/// Generate CSS module class name mappings by hashing class names.
/// Each class name gets a scoped name: `original` → `_original_hash6`.
fn generate_css_module_map(css: &str, file_path: &str) -> Vec<(String, String)> {
    let mut mappings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let chars: Vec<char> = css.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Skip url() content
        if i + 3 < chars.len() && chars[i..i + 3].iter().collect::<String>() == "url" {
            while i < chars.len() && chars[i] != ')' {
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            continue;
        }

        // Skip string literals (content: "...")
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            continue;
        }

        // Check for class selector: . preceded by selector boundary
        if chars[i] == '.' {
            let prev = if i > 0 { chars[i - 1] } else { '\n' };
            let is_selector_start = prev == '{'
                || prev == '}'
                || prev == ','
                || prev == ' '
                || prev == '\n'
                || prev == '\t'
                || prev == '>'
                || prev == '+'
                || prev == '~'
                || i == 0;

            if is_selector_start {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() {
                    let c = chars[end];
                    // Handle escaped characters in class names: .\& or .\31 00.
                    // A backslash escapes either a single character or up to 6
                    // hex digits followed by an optional whitespace terminator.
                    if c == '\\' && end + 1 < chars.len() {
                        end += 1;
                        let mut hex_digits = 0;
                        while end < chars.len() && chars[end].is_ascii_hexdigit() && hex_digits < 6
                        {
                            end += 1;
                            hex_digits += 1;
                        }
                        if hex_digits == 0 {
                            // Single escaped character (e.g. \&, \:, \()
                            end += 1;
                        } else if end < chars.len() && chars[end].is_whitespace() {
                            // Optional whitespace after a hex escape
                            end += 1;
                        }
                        continue;
                    }
                    if c == ':'
                        || c == '['
                        || c == '{'
                        || c == ' '
                        || c == '\n'
                        || c == '\t'
                        || c == '>'
                        || c == '+'
                        || c == '~'
                        || c == ','
                    {
                        break;
                    }
                    end += 1;
                }
                // A hex escape may leave a trailing whitespace terminator in the
                // captured range — it isn't part of the class name.
                let class_name: String = chars[start..end]
                    .iter()
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                // Validate: every char must be alphanumeric/-/_ or escaped
                // (either a backslash itself or the char it escapes).
                let mut escaped = false;
                let is_valid_name = class_name.chars().all(|c| {
                    if escaped {
                        escaped = false;
                        return true;
                    }
                    if c == '\\' {
                        escaped = true;
                        return true;
                    }
                    c.is_alphanumeric() || c == '-' || c == '_'
                });
                if !class_name.is_empty() && is_valid_name && !seen.contains(&class_name) {
                    seen.insert(class_name.clone());
                    let hash_input = format!("{}:{}", file_path, class_name);
                    let hash = blake3::hash(hash_input.as_bytes());
                    let hash_hex = &hash.to_hex()[..6];
                    // Strip backslashes so the scoped name stays a bare identifier.
                    let scoped_base: String = class_name
                        .chars()
                        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                        .collect();
                    let scoped = format!("_{}_{}", scoped_base, hash_hex);
                    mappings.push((class_name, scoped));
                }
                i = end;
                continue;
            }
        }

        i += 1;
    }

    mappings
}

/// Process CSS through a PostCSS-like pipeline
/// Supports Tailwind directives (@tailwind base/components/utilities)
/// and basic PostCSS plugins (autoprefixer is handled by Lightning CSS)
fn process_postcss(source: &str, _file_path: &str) -> String {
    let mut css = source.to_string();

    if css.contains("@tailwind") {
        css = process_tailwind_directives(&css);
    }

    if css.contains("@apply") {
        css = process_tailwind_apply(&css);
    }

    css
}

/// Replace @tailwind directives with generated utility CSS
fn process_tailwind_directives(css: &str) -> String {
    let mut result = css.to_string();

    result = result.replace("@tailwind base;", TAILWIND_BASE);
    result = result.replace("@tailwind base", TAILWIND_BASE);

    result = result.replace("@tailwind components;", TAILWIND_COMPONENTS);
    result = result.replace("@tailwind components", TAILWIND_COMPONENTS);

    result = result.replace("@tailwind utilities;", TAILWIND_UTILITIES);
    result = result.replace("@tailwind utilities", TAILWIND_UTILITIES);

    result
}

/// Process @apply directives (simplified — expands common utilities)
fn process_tailwind_apply(css: &str) -> String {
    let mut result = css.to_string();

    let utilities = [
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
        ("text-center", "text-align: center;"),
        ("text-left", "text-align: left;"),
        ("text-right", "text-align: right;"),
        ("font-bold", "font-weight: 700;"),
        ("font-semibold", "font-weight: 600;"),
        ("font-medium", "font-weight: 500;"),
        ("font-normal", "font-weight: 400;"),
        ("font-light", "font-weight: 300;"),
        ("rounded", "border-radius: 0.25rem;"),
        ("rounded-md", "border-radius: 0.375rem;"),
        ("rounded-lg", "border-radius: 0.5rem;"),
        ("rounded-xl", "border-radius: 0.75rem;"),
        ("rounded-full", "border-radius: 9999px;"),
        ("p-0", "padding: 0;"),
        ("p-1", "padding: 0.25rem;"),
        ("p-2", "padding: 0.5rem;"),
        ("p-3", "padding: 0.75rem;"),
        ("p-4", "padding: 1rem;"),
        ("p-6", "padding: 1.5rem;"),
        ("p-8", "padding: 2rem;"),
        ("m-0", "margin: 0;"),
        ("m-1", "margin: 0.25rem;"),
        ("m-2", "margin: 0.5rem;"),
        ("m-4", "margin: 1rem;"),
        ("m-auto", "margin: auto;"),
        ("mx-auto", "margin-left: auto; margin-right: auto;"),
        ("gap-1", "gap: 0.25rem;"),
        ("gap-2", "gap: 0.5rem;"),
        ("gap-4", "gap: 1rem;"),
        ("gap-6", "gap: 1.5rem;"),
        ("bg-white", "background-color: #fff;"),
        ("bg-black", "background-color: #000;"),
        ("bg-transparent", "background-color: transparent;"),
        ("text-white", "color: #fff;"),
        ("text-black", "color: #000;"),
        ("overflow-hidden", "overflow: hidden;"),
        ("overflow-auto", "overflow: auto;"),
        ("overflow-scroll", "overflow: scroll;"),
        ("cursor-pointer", "cursor: pointer;"),
        ("cursor-default", "cursor: default;"),
        ("relative", "position: relative;"),
        ("absolute", "position: absolute;"),
        ("fixed", "position: fixed;"),
        ("sticky", "position: sticky;"),
        ("top-0", "top: 0;"),
        ("bottom-0", "bottom: 0;"),
        ("left-0", "left: 0;"),
        ("right-0", "right: 0;"),
        ("z-0", "z-index: 0;"),
        ("z-10", "z-index: 10;"),
        ("z-50", "z-index: 50;"),
        ("shadow", "box-shadow: 0 1px 3px rgba(0,0,0,0.1);"),
        ("shadow-md", "box-shadow: 0 4px 6px rgba(0,0,0,0.1);"),
        ("shadow-lg", "box-shadow: 0 10px 15px rgba(0,0,0,0.1);"),
        ("transition", "transition: all 0.15s ease;"),
        ("transition-all", "transition: all 0.15s ease;"),
        ("duration-200", "transition-duration: 200ms;"),
        ("duration-300", "transition-duration: 300ms;"),
    ];

    for (name, props) in &utilities {
        let pattern = format!("@apply {};", name);
        let replacement = format!("/* @apply {} */ {}", name, props);
        result = result.replace(&pattern, &replacement);
    }

    while let Some(start) = result.find("@apply ") {
        let after = &result[start + 7..];
        if let Some(semi) = after.find(';') {
            let utilities_str = &after[..semi];
            let mut expanded = String::new();
            for util in utilities_str.split_whitespace() {
                let found = utilities.iter().find(|(n, _)| *n == util);
                if let Some((_, props)) = found {
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

/// Tailwind base reset CSS
const TAILWIND_BASE: &str = r#"
*, ::before, ::after { box-sizing: border-box; border: 0 solid; }
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
button, [type="button"], [type="reset"], [type="submit"] { -webkit-appearance: button; }
table { border-collapse: collapse; }
"#;

/// Tailwind component classes
const TAILWIND_COMPONENTS: &str = r#"
.container { width: 100%; margin-left: auto; margin-right: auto; }
@media (min-width: 640px) { .container { max-width: 640px; } }
@media (min-width: 768px) { .container { max-width: 768px; } }
@media (min-width: 1024px) { .container { max-width: 1024px; } }
@media (min-width: 1280px) { .container { max-width: 1280px; } }
@media (min-width: 1536px) { .container { max-width: 1536px; } }
"#;

/// Tailwind utility classes (subset)
const TAILWIND_UTILITIES: &str = r#"
.flex { display: flex; }
.inline-flex { display: inline-flex; }
.block { display: block; }
.inline-block { display: inline-block; }
.hidden { display: none; }
.grid { display: grid; }
.items-center { align-items: center; }
.items-start { align-items: flex-start; }
.items-end { align-items: flex-end; }
.justify-center { justify-content: center; }
.justify-between { justify-content: space-between; }
.justify-start { justify-content: flex-start; }
.justify-end { justify-content: flex-end; }
.flex-col { flex-direction: column; }
.flex-row { flex-direction: row; }
.flex-wrap { flex-wrap: wrap; }
.flex-1 { flex: 1 1 0%; }
.w-full { width: 100%; }
.w-auto { width: auto; }
.h-full { height: 100%; }
.h-auto { height: auto; }
.text-center { text-align: center; }
.text-left { text-align: left; }
.text-right { text-align: right; }
.font-bold { font-weight: 700; }
.font-semibold { font-weight: 600; }
.font-medium { font-weight: 500; }
.font-normal { font-weight: 400; }
.rounded { border-radius: 0.25rem; }
.rounded-md { border-radius: 0.375rem; }
.rounded-lg { border-radius: 0.5rem; }
.rounded-xl { border-radius: 0.75rem; }
.rounded-full { border-radius: 9999px; }
.p-0 { padding: 0; }
.p-1 { padding: 0.25rem; }
.p-2 { padding: 0.5rem; }
.p-3 { padding: 0.75rem; }
.p-4 { padding: 1rem; }
.p-6 { padding: 1.5rem; }
.p-8 { padding: 2rem; }
.m-0 { margin: 0; }
.m-4 { margin: 1rem; }
.m-auto { margin: auto; }
.mx-auto { margin-left: auto; margin-right: auto; }
.gap-2 { gap: 0.5rem; }
.gap-4 { gap: 1rem; }
.gap-6 { gap: 1.5rem; }
.bg-white { background-color: #fff; }
.bg-black { background-color: #000; }
.text-white { color: #fff; }
.text-black { color: #000; }
.overflow-hidden { overflow: hidden; }
.overflow-auto { overflow: auto; }
.relative { position: relative; }
.absolute { position: absolute; }
.fixed { position: fixed; }
.sticky { position: sticky; }
.top-0 { top: 0; }
.bottom-0 { bottom: 0; }
.left-0 { left: 0; }
.right-0 { right: 0; }
.z-10 { z-index: 10; }
.z-50 { z-index: 50; }
.shadow { box-shadow: 0 1px 3px rgba(0,0,0,0.1); }
.shadow-md { box-shadow: 0 4px 6px rgba(0,0,0,0.1); }
.shadow-lg { box-shadow: 0 10px 15px rgba(0,0,0,0.1); }
.transition { transition: all 0.15s ease; }
.cursor-pointer { cursor: pointer; }
"#;

pub(super) fn transform_sass(
    source: &str,
    file_path: &str,
    is_production: bool,
    config: &PledgeConfig,
) -> Result<TransformOutput> {
    use grass::{Options, OutputStyle};

    let is_indented = file_path.ends_with(".sass");
    let style = if is_indented {
        grass::InputSyntax::Sass
    } else {
        grass::InputSyntax::Scss
    };

    let output_style = if is_production {
        OutputStyle::Compressed
    } else {
        OutputStyle::Expanded
    };

    let options = Options::default().style(output_style).input_syntax(style);

    let css = grass::from_string(source, &options)
        .map_err(|e| anyhow::anyhow!("Sass compilation error in {}: {}", file_path, e))?;

    let is_css_module = file_path.ends_with(".module.scss") || file_path.ends_with(".module.sass");
    let css_modules = if is_css_module {
        Some(generate_css_module_map(&css, file_path))
    } else {
        None
    };

    let source_map = if config.source_maps {
        Some(crate::css_features::generate_css_source_map(
            file_path, source, &css,
        ))
    } else {
        None
    };

    Ok(TransformOutput {
        code: css,
        source_map,
        css_modules,
        is_css: true,
        extracted_css: None,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}
