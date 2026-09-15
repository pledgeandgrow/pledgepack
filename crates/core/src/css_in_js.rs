// CSS-in-JS compile-time extraction — styled-components, emotion, vanilla-extract.
//
// This module transforms CSS-in-JS patterns at compile time to extract
// static CSS and generate minimal runtime code:
//
// styled-components:
//   const Box = styled.div`color: red;` → .sc-<hash> { color: red; } + const Box = "div"
//
// emotion:
//   const style = css`color: red;` → .css-<hash> { color: red; } + const style = "css-<hash>"
//
// vanilla-extract:
//   const cls = style({ color: 'red' }) → .<hash> { color: red; } + const cls = "<hash>"

use std::collections::HashMap;
use std::path::Path;

/// Detected CSS-in-JS framework
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CssInJsFramework {
    StyledComponents,
    Emotion,
    VanillaExtract,
    None,
}

impl CssInJsFramework {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::StyledComponents => "styled-components",
            Self::Emotion => "emotion",
            Self::VanillaExtract => "vanilla-extract",
            Self::None => "none",
        }
    }
}

/// Detect CSS-in-JS framework from source code
pub fn detect_css_in_js(source: &str) -> CssInJsFramework {
    if (source.contains("styled.") || source.contains("styled("))
        && (source.contains("from 'styled-components'")
            || source.contains("from \"styled-components\""))
    {
        return CssInJsFramework::StyledComponents;
    }
    if source.contains("from '@emotion/") || source.contains("from \"@emotion/") {
        return CssInJsFramework::Emotion;
    }
    if source.contains("from '@vanilla-extract/") || source.contains("from \"@vanilla-extract/") {
        return CssInJsFramework::VanillaExtract;
    }
    CssInJsFramework::None
}

/// Result of CSS-in-JS extraction
pub struct ExtractionResult {
    /// Transformed JS code with CSS extracted
    pub code: String,
    /// Extracted CSS rules
    pub css: String,
    /// Mapping of original identifiers to generated class names
    pub class_names: HashMap<String, String>,
}

/// Find the next occurrence of `needle` in `haystack` at byte offset >= `from`,
/// skipping matches that appear inside string literals or comments.
///
/// Tracks `'`, `"`, and `` ` `` string literals (with `\` escapes), `//` line
/// comments, and `/* */` block comments. The scan always starts at offset 0 so
/// the lexer state is correct even though `from` may be mid-file.
fn find_outside_strings(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let needle_b = needle.as_bytes();
    let n = bytes.len();
    if needle_b.is_empty() || from >= n {
        return None;
    }

    let mut i = 0;
    let mut in_string: Option<u8> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < n {
        // Inside a line comment — ends at newline
        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        // Inside a block comment — ends at `*/`
        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < n && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Inside a string — handle escapes and the closing quote
        if let Some(q) = in_string {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        // Not inside anything — detect comment and string starts
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'/' {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            in_string = Some(bytes[i]);
            i += 1;
            continue;
        }
        // Only match the needle outside strings/comments
        if i >= from && bytes[i..].starts_with(needle_b) {
            return Some(i);
        }
        i += 1;
    }

    None
}

/// Extract CSS from styled-components template literals
/// `styled.div`color: red;`` → `.sc-{hash} { color: red; }` + `const X = "div"`
pub fn extract_styled_components(source: &str, file_path: &str) -> ExtractionResult {
    let mut css = String::new();
    let mut class_names = HashMap::new();
    let mut code = source.to_string();

    // Pattern: styled.tag`...` or styled(Component)`...`
    let patterns = [("styled.", "tag"), ("styled(", "component")];

    for (prefix, mode) in &patterns {
        let mut search_pos = 0;
        while let Some(abs_pos) = find_outside_strings(&code, prefix, search_pos) {
            // Compute positions and extract data without borrowing code
            let after_len = code.len().saturating_sub(abs_pos + prefix.len());
            if after_len == 0 {
                search_pos = abs_pos + prefix.len();
                continue;
            }

            if *mode == "tag" {
                let (tag_name, template, full_end, has_match) = {
                    let after = &code[abs_pos + prefix.len()..];
                    let tag_end = after
                        .find(|c: char| !c.is_alphanumeric() && c != '_')
                        .unwrap_or(after.len());
                    let tag_name = after[..tag_end].to_string();
                    let rest = &after[tag_end..];
                    if let Some(backtick) = rest.find('`') {
                        if let Some(end_tick) = rest[backtick + 1..].find('`') {
                            let template = rest[backtick + 1..backtick + 1 + end_tick].to_string();
                            let full_end =
                                abs_pos + prefix.len() + tag_end + backtick + 1 + end_tick + 1;
                            (tag_name, template, full_end, true)
                        } else {
                            (String::new(), String::new(), 0, false)
                        }
                    } else {
                        (String::new(), String::new(), 0, false)
                    }
                };
                if has_match {
                    let hash = hash_css(file_path, &template);
                    let class_name = format!("sc-{}", hash);
                    css.push_str(&format!(".{} {{{}}}\n", class_name, template.trim()));
                    let replacement = format!("\"{}\"", tag_name);
                    code.replace_range(abs_pos..full_end, &replacement);
                    class_names.insert(tag_name, class_name);
                    search_pos = abs_pos + replacement.len();
                    continue;
                }
            } else if *mode == "component" {
                let (hash, class_name, template, full_end, has_match) = {
                    let after = &code[abs_pos + prefix.len()..];
                    if let Some(close) = after.find(')') {
                        let rest = &after[close + 1..];
                        if let Some(backtick) = rest.find('`') {
                            if let Some(end_tick) = rest[backtick + 1..].find('`') {
                                let template =
                                    rest[backtick + 1..backtick + 1 + end_tick].to_string();
                                let hash = hash_css(file_path, &template);
                                let class_name = format!("sc-{}", hash);
                                let full_end = abs_pos
                                    + prefix.len()
                                    + close
                                    + 1
                                    + backtick
                                    + 1
                                    + end_tick
                                    + 1;
                                (hash, class_name, template, full_end, true)
                            } else {
                                (String::new(), String::new(), String::new(), 0, false)
                            }
                        } else {
                            (String::new(), String::new(), String::new(), 0, false)
                        }
                    } else {
                        (String::new(), String::new(), String::new(), 0, false)
                    }
                };
                if has_match {
                    css.push_str(&format!(".{} {{{}}}\n", class_name, template.trim()));
                    let replacement = format!("\"{}\"", class_name);
                    code.replace_range(abs_pos..full_end, &replacement);
                    class_names.insert(format!("styled_{}", hash), class_name);
                    search_pos = abs_pos + replacement.len();
                    continue;
                }
            }

            search_pos = abs_pos + prefix.len();
        }
    }

    // Process css`` template literal from emotion/styled-components
    code = extract_css_template_calls(&code, file_path, &mut css, &mut class_names);

    ExtractionResult {
        code,
        css,
        class_names,
    }
}

/// Extract CSS from emotion `css`...`` template literals
pub fn extract_emotion(source: &str, file_path: &str) -> ExtractionResult {
    let mut css = String::new();
    let mut class_names = HashMap::new();
    let mut code = source.to_string();

    // Process css`...` template literals
    code = extract_css_template_calls(&code, file_path, &mut css, &mut class_names);

    // Process styled.div`...` (emotion's styled API)
    let styled_result = extract_styled_components(&code, file_path);
    code = styled_result.code;
    css.push_str(&styled_result.css);
    class_names.extend(styled_result.class_names);

    ExtractionResult {
        code,
        css,
        class_names,
    }
}

/// Extract CSS from vanilla-extract style() and globalStyle() calls
pub fn extract_vanilla_extract(source: &str, file_path: &str) -> ExtractionResult {
    let mut css = String::new();
    let mut class_names = HashMap::new();
    let mut code = source.to_string();

    // Pattern: style({ color: 'red', ... })
    let mut search_pos = 0;
    while let Some(abs_pos) = find_outside_strings(&code, "style(", search_pos) {

        let (obj_str, full_end, has_match) = {
            let after = &code[abs_pos + 6..];
            if let Some(brace) = after.find('{') {
                let mut depth = 1;
                let mut end = brace + 1;
                let bytes = after.as_bytes();
                while end < bytes.len() && depth > 0 {
                    match bytes[end] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                let obj_str = after[brace + 1..end - 1].to_string();
                let full_end = abs_pos + 6 + end;
                (obj_str, full_end, true)
            } else {
                (String::new(), 0, false)
            }
        };

        if has_match {
            let hash = hash_css(file_path, &obj_str);
            let class_name = format!("ve-{}", hash);

            let css_rules = js_object_to_css(&obj_str, &class_name);
            css.push_str(&css_rules);

            let replacement = format!("\"{}\"", class_name);
            code.replace_range(abs_pos..full_end, &replacement);

            class_names.insert(format!("style_{}", hash), class_name.clone());
            search_pos = abs_pos + class_name.len() + 2;
        } else {
            search_pos = abs_pos + 6;
        }
    }

    // Pattern: globalStyle(':root', { color: 'red' })
    search_pos = 0;
    while let Some(abs_pos) = find_outside_strings(&code, "globalStyle(", search_pos) {

        let (selector, obj_str, full_end, has_match) = {
            let after = &code[abs_pos + 12..];
            let selector_end = after.find(',').unwrap_or(after.find(')').unwrap_or(0));
            let selector = after[..selector_end]
                .trim()
                .trim_matches(|c| c == '\'' || c == '"' || c == '`')
                .to_string();
            let rest = &after[selector_end..];
            if let Some(brace) = rest.find('{') {
                let mut depth = 1;
                let mut end = brace + 1;
                let bytes = rest.as_bytes();
                while end < bytes.len() && depth > 0 {
                    match bytes[end] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                let obj_str = rest[brace + 1..end - 1].to_string();
                let close_paren = rest[end..].find(')').unwrap_or(0);
                let full_end = abs_pos + 12 + selector_end + end + close_paren + 1;
                (selector, obj_str, full_end, true)
            } else {
                (String::new(), String::new(), 0, false)
            }
        };

        if has_match {
            let css_rules = js_object_to_css(&obj_str, &selector);
            css.push_str(&css_rules);
            code.replace_range(abs_pos..full_end, "undefined");
            search_pos = abs_pos + 9;
        } else {
            search_pos = abs_pos + 12;
        }
    }

    ExtractionResult {
        code,
        css,
        class_names,
    }
}

/// Process css`...` template literal calls
fn extract_css_template_calls(
    code: &str,
    file_path: &str,
    css: &mut String,
    class_names: &mut HashMap<String, String>,
) -> String {
    let mut result = code.to_string();
    let mut search_pos = 0;

    while let Some(abs_pos) = find_outside_strings(&result, "css`", search_pos) {
        let (template, full_end, has_match) = {
            let after = &result[abs_pos + 4..];
            if let Some(end_tick) = after.find('`') {
                let template = after[..end_tick].to_string();
                let full_end = abs_pos + 4 + end_tick + 1;
                (template, full_end, true)
            } else {
                (String::new(), 0, false)
            }
        };

        if has_match {
            let hash = hash_css(file_path, &template);
            let class_name = format!("css-{}", hash);

            css.push_str(&format!(".{} {{{}}}\n", class_name, template.trim()));

            let replacement = format!("\"{}\"", class_name);
            result.replace_range(abs_pos..full_end, &replacement);

            class_names.insert(format!("css_{}", hash), class_name);
            search_pos = abs_pos + replacement.len();
        } else {
            break;
        }
    }

    result
}

/// Convert a JS style object to CSS rules
fn js_object_to_css(obj_str: &str, selector: &str) -> String {
    let mut css = format!("{} {{\n", selector);

    // Split into property declarations by commas and newlines
    // This handles both multi-line and single-line objects
    let parts: Vec<&str> = obj_str.split([',', '\n']).collect();
    for part in parts {
        let trimmed = part.trim();
        if let Some(colon) = trimmed.find(':') {
            let key = trimmed[..colon]
                .trim()
                .trim_matches(|c| c == '\'' || c == '"');
            let value = trimmed[colon + 1..]
                .trim()
                .trim_matches(|c: char| c == '\'' || c == '"')
                .trim_end_matches(',');

            if !key.is_empty() && !value.is_empty() {
                let css_key = camel_to_kebab(key);
                css.push_str(&format!("  {}: {};\n", css_key, value));
            }
        }
    }

    css.push_str("}\n");
    css
}

/// Convert camelCase to kebab-case
fn camel_to_kebab(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('-');
            }
            result.push(c.to_lowercase().next().unwrap_or(c));
        } else {
            result.push(c);
        }
    }
    result
}

/// Generate a short hash for CSS class names
fn hash_css(file_path: &str, content: &str) -> String {
    let input = format!("{}:{}", file_path, content);
    let hash = blake3::hash(input.as_bytes());
    hash.to_hex()[..8].to_string()
}

/// Main entry point: detect and extract CSS-in-JS from a source file
pub fn extract_css_in_js(source: &str, file_path: &str) -> Option<ExtractionResult> {
    let framework = detect_css_in_js(source);

    match framework {
        CssInJsFramework::StyledComponents => {
            let result = extract_styled_components(source, file_path);
            if result.css.is_empty() {
                None
            } else {
                Some(result)
            }
        }
        CssInJsFramework::Emotion => {
            let result = extract_emotion(source, file_path);
            if result.css.is_empty() {
                None
            } else {
                Some(result)
            }
        }
        CssInJsFramework::VanillaExtract => {
            let result = extract_vanilla_extract(source, file_path);
            if result.css.is_empty() {
                None
            } else {
                Some(result)
            }
        }
        CssInJsFramework::None => None,
    }
}

/// #73: Tree-shake CSS-in-JS runtime after static extraction.
/// Strips runtime imports from the code when all styles have been
/// extracted at build time, achieving zero-runtime CSS-in-JS.
pub fn tree_shake_runtime(
    source: &str,
    framework: CssInJsFramework,
    extracted_css: &str,
    dynamic_styles: &[String],
) -> String {
    if framework == CssInJsFramework::None {
        return source.to_string();
    }

    if !crate::performance::can_strip_runtime(extracted_css, dynamic_styles) {
        return source.to_string();
    }

    crate::performance::strip_css_in_js_runtime(source, framework.as_str())
}

/// Detect if a project uses CSS-in-JS by checking package.json
pub fn detect_framework_in_project(root: &Path) -> CssInJsFramework {
    let pkg_path = root.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_path) {
        if content.contains("\"styled-components\"") {
            return CssInJsFramework::StyledComponents;
        }
        if content.contains("\"@emotion/") || content.contains("\"emotion\"") {
            return CssInJsFramework::Emotion;
        }
        if content.contains("\"@vanilla-extract/") {
            return CssInJsFramework::VanillaExtract;
        }
    }
    CssInJsFramework::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_styled_components() {
        let source = "import styled from 'styled-components'\nconst Box = styled.div`color: red;`";
        assert_eq!(detect_css_in_js(source), CssInJsFramework::StyledComponents);
    }

    #[test]
    fn test_detect_emotion() {
        let source = "import { css } from '@emotion/react'\nconst style = css`color: red;`";
        assert_eq!(detect_css_in_js(source), CssInJsFramework::Emotion);
    }

    #[test]
    fn test_extract_styled_div() {
        let source = "const Box = styled.div`color: red; padding: 10px;`";
        let result = extract_styled_components(source, "test.tsx");
        assert!(result.css.contains("color: red"));
        assert!(result.css.contains("padding: 10px"));
        assert!(result.code.contains("\"div\""));
    }

    #[test]
    fn test_extract_emotion_css() {
        let source = "const style = css`color: red;`";
        let result = extract_emotion(source, "test.tsx");
        assert!(result.css.contains("color: red"));
        assert!(result.code.contains("css-"));
    }

    #[test]
    fn test_camel_to_kebab() {
        assert_eq!(camel_to_kebab("backgroundColor"), "background-color");
        assert_eq!(camel_to_kebab("color"), "color");
        assert_eq!(camel_to_kebab("marginTop"), "margin-top");
    }

    #[test]
    fn test_vanilla_extract_style() {
        let source = "const cls = style({ color: 'red', backgroundColor: 'blue' })";
        let result = extract_vanilla_extract(source, "test.css.ts");
        assert!(result.css.contains("color: red"));
        assert!(result.css.contains("background-color: blue"));
        assert!(result.code.contains("ve-"));
    }
}
