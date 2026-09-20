// Auto-generates HTML shell and entry module from layout.tsx
//
// Instead of requiring a static index.html + entry.tsx, the dev server:
//   1. Scans app/layout.tsx for <html>, <head>, <body> structure
//   2. Extracts <meta>, <title>, <link>, <style> from layout's <head>
//   3. Generates an HTML shell with HMR client injected
//   4. Generates an in-memory entry module with route-aware code splitting
//
// This eliminates the need for index.html and entry.tsx in project templates.

use std::path::Path;
use tracing::{info, warn};

/// Extract HTML shell structure from a layout.tsx file.
///
/// Parses the JSX return statement to find:
/// - <html lang="..."> attributes
/// - <head> contents (title, meta, link, style tags)
/// - <body> tag (used as mount container)
///
/// Returns (html_attrs, head_content) where:
/// - html_attrs: e.g., r#"lang="en" class="dark""#
/// - head_content: raw HTML string for <head>
pub fn extract_shell_from_layout(layout_source: &str) -> (String, String) {
    let mut html_attrs = String::new();
    let mut head_content = String::new();

    // Extract <html ...> attributes
    if let Some(html_start) = layout_source.find("<html") {
        let after_html = &layout_source[html_start + 5..];
        if let Some(tag_end) = after_html.find('>') {
            let attrs_raw = &after_html[..tag_end];
            // Parse attributes from JSX (handle both " and ' quotes)
            html_attrs = parse_jsx_attributes(attrs_raw);
        }
    }

    // Extract <head>...</head> contents
    if let Some(head_start) = layout_source.find("<head>") {
        if let Some(head_end) = layout_source.find("</head>") {
            let head_raw = &layout_source[head_start + 6..head_end];
            // Convert JSX head content to HTML
            head_content = jsx_head_to_html(head_raw);
        }
    } else if let Some(head_start) = layout_source.find("<head ") {
        // Handle <head className="...">
        if let Some(tag_end) = layout_source[head_start..].find('>') {
            let real_head_start = head_start + tag_end + 1;
            if let Some(head_end) = layout_source[real_head_start..].find("</head>") {
                let head_raw = &layout_source[real_head_start..real_head_start + head_end];
                head_content = jsx_head_to_html(head_raw);
            }
        }
    }

    (html_attrs, head_content)
}

/// Parse JSX attributes from a tag string into HTML attribute string
fn parse_jsx_attributes(attrs: &str) -> String {
    let mut result = Vec::new();
    let mut chars = attrs.chars().peekable();
    let mut current = String::new();
    let mut in_string = false;
    let mut string_char = '\0';

    while let Some(c) = chars.next() {
        if in_string {
            current.push(c);
            if c == string_char
                && current.chars().nth(current.len().saturating_sub(2)) != Some('\\')
            {
                in_string = false;
            }
        } else if c == '"' || c == '\'' {
            in_string = true;
            string_char = c;
            current.push(c);
        } else if c.is_whitespace() {
            if !current.is_empty() {
                // Convert JSX-specific attributes
                let converted = convert_jsx_attr(&current);
                result.push(converted);
                current = String::new();
            }
        } else if c == '{' {
            // JSX expression — skip until matching }
            let mut depth = 1;
            current.push('{');
            while depth > 0 {
                if let Some(nc) = chars.next() {
                    current.push(nc);
                    if nc == '{' {
                        depth += 1;
                    }
                    if nc == '}' {
                        depth -= 1;
                    }
                } else {
                    break;
                }
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        let converted = convert_jsx_attr(&current);
        result.push(converted);
    }

    result
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Convert a JSX attribute to HTML (e.g., className → class, htmlFor → for)
fn convert_jsx_attr(attr: &str) -> String {
    let trimmed = attr.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Handle key=value format
    if let Some(eq_pos) = trimmed.find('=') {
        let key = trimmed[..eq_pos].trim();
        let value = trimmed[eq_pos + 1..].trim();

        // Convert JSX attribute names to HTML
        let html_key = match key {
            "className" => "class",
            "htmlFor" => "for",
            "tabIndex" => "tabindex",
            "readOnly" => "readonly",
            "maxLength" => "maxlength",
            "cellSpacing" => "cellspacing",
            "rowSpan" => "rowspan",
            "colSpan" => "colspan",
            "useMap" => "usemap",
            "frameBorder" => "frameborder",
            "accessKey" => "accesskey",
            "contentEditable" => "contenteditable",
            "contextMenu" => "contextmenu",
            "spellCheck" => "spellcheck",
            other => other,
        };

        // Remove braces from JSX expressions like {true}
        let clean_value = value
            .trim_matches(|c| c == '{' || c == '}')
            .trim_matches('"')
            .trim_matches('\'')
            .trim();

        // Skip boolean attributes that are false
        if clean_value == "false" {
            return String::new();
        }

        // For boolean true, just output the attribute name
        if clean_value == "true" {
            return html_key.to_string();
        }

        format!(r#"{}="{}""#, html_key, clean_value)
    } else {
        // Boolean attribute without value
        trimmed.to_string()
    }
}

/// Convert JSX head content to HTML
fn jsx_head_to_html(head_jsx: &str) -> String {
    let mut html = String::new();
    let mut chars = head_jsx.chars().peekable();
    let mut current_tag = String::new();
    let mut in_tag = false;

    while let Some(c) = chars.next() {
        if !in_tag && c == '<' {
            in_tag = true;
            current_tag.clear();
            current_tag.push(c);
        } else if in_tag {
            current_tag.push(c);
            if c == '>' {
                in_tag = false;
                // Convert the JSX tag to HTML
                let converted = convert_jsx_tag_to_html(&current_tag);
                html.push_str(&converted);
                current_tag.clear();
            }
        } else if c == '{' {
            // JSX expression in head — skip (e.g., {children})
            let mut depth = 1;
            while depth > 0 {
                if let Some(nc) = chars.next() {
                    if nc == '{' {
                        depth += 1;
                    }
                    if nc == '}' {
                        depth -= 1;
                    }
                } else {
                    break;
                }
            }
        } else {
            // Text content between tags
            html.push(c);
        }
    }

    // Clean up whitespace
    html.split('\n')
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n    ")
}

/// Convert a single JSX tag to HTML
fn convert_jsx_tag_to_html(tag: &str) -> String {
    // Self-closing tags
    if let Some(inner) = tag.strip_suffix("/>") {
        let attrs = parse_jsx_attributes(&inner[1..]);
        let tag_name = inner[1..].split_whitespace().next().unwrap_or("");
        format!("<{} {} />", tag_name, attrs)
    } else if tag.starts_with("</") {
        // Closing tag
        let name = tag[2..tag.len() - 1].trim();
        format!("</{}>", name)
    } else {
        // Opening tag
        let inner = &tag[1..tag.len() - 1];
        let attrs = parse_jsx_attributes(inner);
        let tag_name = inner.split_whitespace().next().unwrap_or("");
        if attrs.is_empty() {
            format!("<{}>", tag_name)
        } else {
            format!("<{} {}>", tag_name, attrs)
        }
    }
}

/// Generate the complete HTML shell with HMR client injected
pub fn generate_html_shell(
    html_attrs: &str,
    head_content: &str,
    hmr_script: &str,
    import_map: &str,
) -> String {
    generate_html_shell_with_base(html_attrs, head_content, hmr_script, import_map, "/")
}

/// Generate the complete HTML shell with HMR client and base path support
pub fn generate_html_shell_with_base(
    html_attrs: &str,
    head_content: &str,
    hmr_script: &str,
    import_map: &str,
    base: &str,
) -> String {
    let base = base.trim_end_matches('/');
    let entry_src = format!("{}/__pledge_entry", base);

    let html_open = if html_attrs.is_empty() {
        "<html>".to_string()
    } else {
        format!("<html {}>", html_attrs)
    };

    let import_map_tag = if import_map.is_empty() {
        String::new()
    } else {
        format!(
            "<script type=\"importmap\">\n{}\n</script>\n    ",
            import_map
        )
    };

    format!(
        r#"<!DOCTYPE html>
{html_open}
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    {import_map_tag}{head_content}
  </head>
  <body>
    <div id="root"></div>
    <script type="module" src="{entry_src}"></script>
{hmr_script}
  </body>
</html>"#,
        html_open = html_open,
        import_map_tag = import_map_tag,
        head_content = if head_content.is_empty() {
            "<title>PledgeStack</title>".to_string()
        } else {
            head_content.to_string()
        },
        entry_src = entry_src,
        hmr_script = hmr_script,
    )
}

/// Generate the in-memory entry module.
///
/// This replaces a static entry.tsx file. It includes:
/// - React root creation
/// - Route-aware rendering via /__pledge_router
/// - SPA navigation (link click interception)
/// - HMR fast refresh integration
/// - Hot head swapping (meta/title updates without page reload)
pub fn generate_entry_module() -> String {
    r#"// Auto-generated by Pledge dev server
// In-memory entry module with route rendering, SPA navigation, and HMR.

import React from "react";
import { createRoot } from "react-dom/client";
import { render } from "/__pledge_router";

var root = createRoot(document.getElementById("root"));

function renderApp() {
  var pathname = window.location.pathname;
  var element = render(pathname);
  root.render(element);
}

renderApp();

window.addEventListener("popstate", renderApp);

document.addEventListener("click", function(e) {
  var target = e.target;
  var anchor = target.closest && target.closest("a");
  if (anchor && anchor.href.startsWith(window.location.origin) && !anchor.target) {
    e.preventDefault();
    var url = new URL(anchor.href);
    window.history.pushState({}, "", url.pathname);
    renderApp();
  }
});

window.__pledge_fast_refresh = window.__pledge_fast_refresh || function(name, reload) {
  (window.__pledge_fast_refresh_registry = window.__pledge_fast_refresh_registry || {})[name] = reload;
};
window.__pledge_fast_refresh.render = renderApp;
"#
    .to_string()
}

/// Try to read and parse layout.tsx from the app directory.
/// Returns (html_attrs, head_content) if successful, None otherwise.
pub fn try_extract_shell_from_project(root: &Path) -> Option<(String, String)> {
    // Find layout.tsx in app/ or src/app/
    let candidates = [
        root.join("app").join("layout.tsx"),
        root.join("app").join("layout.ts"),
        root.join("app").join("layout.jsx"),
        root.join("app").join("layout.js"),
        root.join("src").join("app").join("layout.tsx"),
        root.join("src").join("app").join("layout.ts"),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            match std::fs::read_to_string(candidate) {
                Ok(source) => {
                    info!("Auto-generating HTML shell from {}", pledgepack_core::display_path(&candidate));
                    let (attrs, head) = extract_shell_from_layout(&source);
                    return Some((attrs, head));
                }
                Err(e) => {
                    warn!("Failed to read {}: {}", pledgepack_core::display_path(&candidate), e);
                }
            }
        }
    }

    None
}
