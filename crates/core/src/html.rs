// HTML processing — index.html as entry point
//
// Parses index.html to extract:
//   - <script type="module" src="..."> → entry points
//   - <link rel="stylesheet" href="..."> → CSS dependencies
//   - <meta> tags → metadata
//   - <title> → page title
//
// During build, processes the HTML to:
//   - Replace script src with hashed output filenames
//   - Inject CSS <link> tags for extracted styles
//   - Add preload/prefetch hints
//   - Inject meta tags from config

use anyhow::Result;
use scraper::{Html, Selector};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::info;

/// Parsed HTML entry point information
#[derive(Debug, Clone)]
pub struct HtmlEntry {
    /// Script src paths found in the HTML (e.g., ["/src/index.tsx"])
    pub scripts: Vec<String>,
    /// CSS stylesheet links found in the HTML
    pub stylesheets: Vec<String>,
    /// Module preload scripts
    pub module_preloads: Vec<String>,
    /// Page title
    pub title: Option<String>,
    /// Meta tags (name → content)
    pub meta: HashMap<String, String>,
    /// Raw HTML content
    pub html: String,
}

/// Process an HTML file as an entry point
pub fn process_html(html_path: &PathBuf) -> Result<HtmlEntry> {
    let html = std::fs::read_to_string(html_path)?;

    let scripts = extract_script_srcs(&html);
    let stylesheets = extract_stylesheet_links(&html);
    let module_preloads = extract_module_preloads(&html);
    let title = extract_title(&html);
    let meta = extract_meta_tags(&html);

    info!(
        "HTML entry: {} scripts, {} stylesheets, {} preloads",
        scripts.len(),
        stylesheets.len(),
        module_preloads.len()
    );

    Ok(HtmlEntry {
        scripts,
        stylesheets,
        module_preloads,
        title,
        meta,
        html,
    })
}

/// Generate production HTML with hashed asset references
/// Supports multiple entry scripts, CSS links, module preloads, preload/prefetch hints
pub fn generate_production_html(
    template_html: &str,
    entry_scripts: &[(String, String)], // (original_src, hashed_filename)
    css_files: &[String],
    additional_meta: &HashMap<String, String>,
) -> String {
    generate_production_html_with_base(
        template_html,
        entry_scripts,
        css_files,
        additional_meta,
        "/",
    )
}

/// Generate production HTML with hashed asset references and base path
pub fn generate_production_html_with_base(
    template_html: &str,
    entry_scripts: &[(String, String)],
    css_files: &[String],
    additional_meta: &HashMap<String, String>,
    base: &str,
) -> String {
    let base = base.trim_end_matches('/');
    let prefix = if base.is_empty() {
        String::new()
    } else {
        base.to_string()
    };

    let mut html = template_html.to_string();

    // Replace script src references with hashed versions
    for (original, hashed) in entry_scripts {
        let old_src = format!(r#"src="{}""#, original);
        let new_src = format!(r#"src="{}/{}""#, prefix, hashed);
        html = html.replace(&old_src, &new_src);
    }

    // Inject CSS <link> tags before </head>
    let css_links: String = css_files
        .iter()
        .map(|css| format!(r#"    <link rel="stylesheet" href="{}/{}" />"#, prefix, css))
        .collect::<Vec<_>>()
        .join("\n");

    if !css_links.is_empty() {
        html = html.replace("</head>", &format!("{}\n</head>", css_links));
    }

    // Inject additional meta tags
    let meta_tags: String = additional_meta
        .iter()
        .map(|(name, content)| format!(r#"    <meta name="{}" content="{}" />"#, name, content))
        .collect::<Vec<_>>()
        .join("\n");

    if !meta_tags.is_empty() {
        html = html.replace("</head>", &format!("{}\n</head>", meta_tags));
    }

    html
}

/// Generate production HTML with full preload/prefetch and modulepreload support
#[allow(clippy::too_many_arguments)]
pub fn generate_production_html_with_preloads(
    template_html: &str,
    entry_scripts: &[(String, String)],
    css_files: &[String],
    async_chunks: &[String],
    module_preload: bool,
    preload: bool,
    prefetch: bool,
    additional_meta: &HashMap<String, String>,
) -> String {
    generate_production_html_with_preloads_and_base(
        template_html,
        entry_scripts,
        css_files,
        async_chunks,
        module_preload,
        preload,
        prefetch,
        additional_meta,
        "/",
    )
}

/// Generate production HTML with full preload/prefetch, modulepreload, and base path support
#[allow(clippy::too_many_arguments)]
pub fn generate_production_html_with_preloads_and_base(
    template_html: &str,
    entry_scripts: &[(String, String)],
    css_files: &[String],
    async_chunks: &[String],
    module_preload: bool,
    preload: bool,
    prefetch: bool,
    additional_meta: &HashMap<String, String>,
    base: &str,
) -> String {
    let base = base.trim_end_matches('/');
    let prefix = if base.is_empty() {
        String::new()
    } else {
        base.to_string()
    };

    let mut html = template_html.to_string();

    // Replace script src references with hashed versions for all entry scripts
    for (original, hashed) in entry_scripts {
        let old_src = format!(r#"src="{}""#, original);
        let new_src = format!(r#"src="{}/{}""#, prefix, hashed);
        html = html.replace(&old_src, &new_src);
    }

    let mut head_injections = String::new();

    // CSS links
    for css in css_files {
        head_injections.push_str(&format!(
            r#"    <link rel="stylesheet" href="{}/{}" />"#,
            prefix, css
        ));
        head_injections.push('\n');
    }

    // Module preload directives for async chunks
    if module_preload {
        for chunk in async_chunks {
            head_injections.push_str(&format!(
                r#"    <link rel="modulepreload" href="{}/{}" />"#,
                prefix, chunk
            ));
            head_injections.push('\n');
        }
    }

    // Preload directives for critical assets (first CSS file)
    if preload && !css_files.is_empty() {
        head_injections.push_str(&format!(
            r#"    <link rel="preload" href="{}/{}" as="style" />"#,
            prefix, css_files[0]
        ));
        head_injections.push('\n');
    }

    // Prefetch directives for async chunks
    if prefetch {
        for chunk in async_chunks {
            head_injections.push_str(&format!(
                r#"    <link rel="prefetch" href="{}/{}" />"#,
                prefix, chunk
            ));
            head_injections.push('\n');
        }
    }

    // Meta tags
    for (name, content) in additional_meta {
        head_injections.push_str(&format!(
            r#"    <meta name="{}" content="{}" />"#,
            name, content
        ));
        head_injections.push('\n');
    }

    if !head_injections.is_empty() {
        html = html.replace("</head>", &format!("{}\n</head>", head_injections));
    }

    html
}

/// Generate a default index.html if none exists
pub fn generate_default_html(entry: &str, title: &str) -> String {
    generate_default_html_with_base(entry, title, "/")
}

/// Generate a default index.html with base path support
pub fn generate_default_html_with_base(entry: &str, title: &str, base: &str) -> String {
    let base = base.trim_end_matches('/');
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>{title}</title>
</head>
<body>
    <div id="root"></div>
    <script type="module" src="{base}/{entry}"></script>
</body>
</html>"#,
        title = title,
        base = base,
        entry = entry,
    )
}

/// Extract <script type="module" src="..."> src paths
/// Uses the `scraper` crate for proper HTML parsing — handles attributes
/// containing `>`, self-closing tags, single-quoted and unquoted attributes.
fn extract_script_srcs(html: &str) -> Vec<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("script[src]").unwrap();
    document
        .select(&selector)
        .filter(|el| {
            el.value()
                .attr("type")
                .map(|t| t.eq_ignore_ascii_case("module"))
                .unwrap_or(false)
        })
        .filter_map(|el| el.value().attr("src"))
        .map(|s| s.to_string())
        .collect()
}

/// Extract <link rel="stylesheet" href="..."> href paths
fn extract_stylesheet_links(html: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut search_pos = 0;

    while let Some(pos) = html[search_pos..].find("<link") {
        let abs_pos = search_pos + pos;
        let rest = &html[abs_pos..];

        if let Some(end) = rest.find('>') {
            let tag = &rest[..end];

            if tag.contains(r#"rel="stylesheet""#) || tag.contains("rel='stylesheet'") {
                if let Some(href_start) = tag.find(r#"href=""#) {
                    let href_rest = &tag[href_start + 6..];
                    if let Some(href_end) = href_rest.find('"') {
                        links.push(href_rest[..href_end].to_string());
                    }
                } else if let Some(href_start) = tag.find("href='") {
                    let href_rest = &tag[href_start + 6..];
                    if let Some(href_end) = href_rest.find('\'') {
                        links.push(href_rest[..href_end].to_string());
                    }
                }
            }
        }

        search_pos = abs_pos + 1;
    }

    links
}

/// Extract <link rel="modulepreload" href="..."> href paths
fn extract_module_preloads(html: &str) -> Vec<String> {
    let mut preloads = Vec::new();
    let mut search_pos = 0;

    while let Some(pos) = html[search_pos..].find("<link") {
        let abs_pos = search_pos + pos;
        let rest = &html[abs_pos..];

        if let Some(end) = rest.find('>') {
            let tag = &rest[..end];

            if tag.contains("modulepreload")
                && let Some(href_start) = tag.find(r#"href=""#)
            {
                let href_rest = &tag[href_start + 6..];
                if let Some(href_end) = href_rest.find('"') {
                    preloads.push(href_rest[..href_end].to_string());
                }
            }
        }

        search_pos = abs_pos + 1;
    }

    preloads
}

/// Extract <title>...</title>
/// Uses `scraper` so <title> tags with attributes (e.g. <title id="...">)
/// are matched correctly.
fn extract_title(html: &str) -> Option<String> {
    let document = Html::parse_document(html);
    let selector = Selector::parse("title").unwrap();
    document
        .select(&selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
}

/// Extract <meta name="..." content="..."> tags
fn extract_meta_tags(html: &str) -> HashMap<String, String> {
    let mut meta = HashMap::new();
    let mut search_pos = 0;

    while let Some(pos) = html[search_pos..].find("<meta") {
        let abs_pos = search_pos + pos;
        let rest = &html[abs_pos..];

        if let Some(end) = rest.find('>') {
            let tag = &rest[..end];

            let name = extract_attr(tag, "name");
            let content = extract_attr(tag, "content");

            if let (Some(n), Some(c)) = (name, content) {
                meta.insert(n, c);
            }
        }

        search_pos = abs_pos + 1;
    }

    meta
}

/// Extract an attribute value from an HTML tag
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!(r#"{}=""#, attr);
    if let Some(start) = tag.find(&pattern) {
        let rest = &tag[start + pattern.len()..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }

    let pattern = format!("{}='", attr);
    if let Some(start) = tag.find(&pattern) {
        let rest = &tag[start + pattern.len()..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_string());
        }
    }

    None
}

/// Minify HTML for production — removes comments, whitespace, redundant attributes
pub fn minify_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_comment = false;
    let mut prev_char = '\0';
    let mut chars = html.chars().peekable();

    while let Some(c) = chars.next() {
        if in_comment {
            // Check for end of comment -->
            if c == '-' && chars.peek() == Some(&'-') {
                chars.next();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    in_comment = false;
                }
            }
            continue;
        }

        if c == '<' {
            result.push(c);
            // Check for HTML comment: last emitted char is '<' followed by "!--"
            if result.ends_with('<') && chars.peek() == Some(&'!') {
                // Look ahead for <!--
                let mut lookahead = chars.clone();
                lookahead.next(); // !
                if lookahead.next() == Some('-') && lookahead.next() == Some('-') {
                    // Strip the comment entirely — remove the '<' we just pushed
                    result.pop();
                    chars.next(); // consume !
                    chars.next(); // consume -
                    chars.next(); // consume -
                    in_comment = true;
                    continue;
                }
            }
            in_tag = true;
            continue;
        }

        if c == '>' && in_tag {
            in_tag = false;
            result.push(c);
            prev_char = '>';
            continue;
        }

        if in_tag {
            result.push(c);
            continue;
        }

        // Outside tags: collapse whitespace
        if c.is_whitespace() {
            if prev_char != ' ' && prev_char != '\n' && prev_char != '\t' && prev_char != '\r' {
                result.push(' ');
                prev_char = ' ';
            }
        } else {
            result.push(c);
            prev_char = c;
        }
    }

    // Remove spaces between tags
    result = result.replace("> <", "><");

    // Remove trailing whitespace before </tag>
    result = result.replace("  </", "</");
    result = result.replace(" </", "</");

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_script_srcs() {
        let html = r#"
            <script type="module" src="/src/index.tsx"></script>
            <script src="legacy.js"></script>
            <script type="module" src="/src/app.ts"></script>
        "#;
        let scripts = extract_script_srcs(html);
        assert_eq!(scripts, vec!["/src/index.tsx", "/src/app.ts"]);
    }

    #[test]
    fn test_extract_title() {
        let html = "<html><head><title>My App</title></head></html>";
        assert_eq!(extract_title(html), Some("My App".to_string()));
    }

    #[test]
    fn test_extract_meta_tags() {
        let html = r#"<meta name="viewport" content="width=device-width" />"#;
        let meta = extract_meta_tags(html);
        assert_eq!(
            meta.get("viewport"),
            Some(&"width=device-width".to_string())
        );
    }
}
