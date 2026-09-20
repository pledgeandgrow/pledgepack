// Asset pipeline — MDX, GraphQL, YAML/CSV/TSV, image auto-selection,
// audio/video, PDF, and asset manifest generation.
//
// Features 31-37 from the roadmap.

use std::collections::HashMap;
use std::path::Path;

/// Read an asset file as bytes, using memory-mapped I/O for large files (>64KB).
/// This speeds up loading of large binary assets like images, videos, and PDFs.
pub fn read_asset_mmap(path: &Path) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > 65536 {
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(mmap.as_ref().to_vec())
    } else {
        std::fs::read(path)
    }
}

/// Read an asset file as a string, using memory-mapped I/O for large files (>64KB).
pub fn read_asset_text_mmap(path: &Path) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > 65536 {
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Ok(String::from_utf8_lossy(mmap.as_ref()).into_owned())
    } else {
        std::fs::read_to_string(path)
    }
}

/// Discover asset files in a directory tree matching the given glob patterns.
/// Uses globset for efficient pattern matching.
pub fn discover_assets(root: &Path, patterns: &[String]) -> Vec<std::path::PathBuf> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = globset::Glob::new(pattern) {
            builder.add(glob);
        }
    }
    let glob_set = builder.build().unwrap_or_default();

    let mut results = Vec::new();
    walk_for_assets(root, &glob_set, &mut results);
    results.sort();
    results
}

fn walk_for_assets(dir: &Path, glob_set: &globset::GlobSet, results: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "node_modules" || name == "target" || name.starts_with('.') {
                    continue;
                }
                walk_for_assets(&path, glob_set, results);
            } else if path.is_file() {
                let rel = path.strip_prefix(dir).unwrap_or(&path);
                let rel_str = crate::normalize_path(rel);
                if glob_set.is_match(&rel_str) {
                    results.push(path);
                }
            }
        }
    }
}

// ─── Feature 31: MDX compilation ──────────────────────────────────────

/// MDX frontmatter and content
#[derive(Debug, Clone, Default)]
pub struct MdxResult {
    /// Frontmatter as key-value pairs
    pub frontmatter: HashMap<String, String>,
    /// Transformed JSX code
    pub code: String,
}

/// Compile MDX (Markdown + JSX) to JavaScript module
/// Extracts frontmatter, converts markdown to JSX, preserves JSX components
pub fn compile_mdx(source: &str, file_path: &str) -> MdxResult {
    let mut result = MdxResult::default();
    let mut content = source.to_string();

    // Extract frontmatter (--- at the start of the file)
    if content.starts_with("---")
        && let Some(end) = content[3..].find("---")
    {
        let frontmatter_str = &content[3..3 + end];
        for line in frontmatter_str.lines() {
            let trimmed = line.trim();
            if let Some(colon) = trimmed.find(':') {
                let key = trimmed[..colon].trim().to_string();
                let value = trimmed[colon + 1..]
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string();
                if !key.is_empty() {
                    result.frontmatter.insert(key, value);
                }
            }
        }
        content = content[3 + end + 3..].trim_start().to_string();
    }

    // Convert markdown to JSX
    let jsx = markdown_to_jsx(&content, file_path);
    result.code = generate_mdx_module(&jsx, &result.frontmatter);
    result
}

/// Convert markdown content to JSX
fn markdown_to_jsx(md: &str, _file_path: &str) -> String {
    let mut jsx = String::new();
    let mut in_code_block = false;
    let mut code_block_lang = String::new();
    let mut code_block_content = String::new();
    let mut in_list = false;

    for line in md.lines() {
        // Code block fence
        if line.trim_start().starts_with("```") {
            if in_code_block {
                // End code block
                jsx.push_str(&format!(
                    "<pre><code class=\"language-{}\">{}</code></pre>\n",
                    code_block_lang,
                    escape_jsx(&code_block_content)
                ));
                in_code_block = false;
                code_block_content.clear();
                code_block_lang.clear();
            } else {
                // Start code block
                in_code_block = true;
                code_block_lang = line.trim_start()[3..].trim().to_string();
            }
            continue;
        }

        if in_code_block {
            code_block_content.push_str(line);
            code_block_content.push('\n');
            continue;
        }

        // Headings
        if let Some(rest) = line.trim_start().strip_prefix("# ") {
            jsx.push_str(&format!("<h1>{}</h1>\n", inline_markdown(rest)));
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("## ") {
            jsx.push_str(&format!("<h2>{}</h2>\n", inline_markdown(rest)));
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("### ") {
            jsx.push_str(&format!("<h3>{}</h3>\n", inline_markdown(rest)));
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("#### ") {
            jsx.push_str(&format!("<h4>{}</h4>\n", inline_markdown(rest)));
            continue;
        }

        // List items
        if line.trim_start().starts_with("- ") || line.trim_start().starts_with("* ") {
            if !in_list {
                jsx.push_str("<ul>\n");
                in_list = true;
            }
            let item = line.trim_start()[2..].trim();
            jsx.push_str(&format!("  <li>{}</li>\n", inline_markdown(item)));
            continue;
        } else if in_list {
            jsx.push_str("</ul>\n");
            in_list = false;
        }

        // Blockquotes
        if let Some(rest) = line.trim_start().strip_prefix("> ") {
            jsx.push_str(&format!(
                "<blockquote>{}</blockquote>\n",
                inline_markdown(rest)
            ));
            continue;
        }

        // Horizontal rule
        if line.trim() == "---" || line.trim() == "***" || line.trim() == "___" {
            jsx.push_str("<hr />\n");
            continue;
        }

        // Empty line
        if line.trim().is_empty() {
            continue;
        }

        // Paragraph (check for JSX elements first)
        let trimmed = line.trim();
        if trimmed.starts_with('<') {
            // JSX element — pass through
            jsx.push_str(trimmed);
            jsx.push('\n');
        } else {
            jsx.push_str(&format!("<p>{}</p>\n", inline_markdown(trimmed)));
        }
    }

    if in_list {
        jsx.push_str("</ul>\n");
    }

    jsx
}

/// Process inline markdown (bold, italic, code, links)
fn inline_markdown(text: &str) -> String {
    let mut result = text.to_string();

    // Bold: **text** or __text__
    while let Some(start) = result.find("**") {
        if let Some(end) = result[start + 2..].find("**") {
            let inner = &result[start + 2..start + 2 + end];
            let replacement = format!("<strong>{}</strong>", inner);
            result.replace_range(start..start + 2 + end + 2, &replacement);
        } else {
            break;
        }
    }

    // Italic: *text* or _text_
    while let Some(start) = result.find('*') {
        if let Some(end) = result[start + 1..].find('*') {
            let inner = &result[start + 1..start + 1 + end];
            let replacement = format!("<em>{}</em>", inner);
            result.replace_range(start..start + 1 + end + 1, &replacement);
        } else {
            break;
        }
    }

    // Inline code: `code`
    while let Some(start) = result.find('`') {
        if let Some(end) = result[start + 1..].find('`') {
            let inner = &result[start + 1..start + 1 + end];
            let replacement = format!("<code>{}</code>", escape_jsx(inner));
            result.replace_range(start..start + 1 + end + 1, &replacement);
        } else {
            break;
        }
    }

    // Links: [text](url)
    while let Some(start) = result.find('[') {
        if let Some(text_end) = result[start..].find("](") {
            if let Some(url_end) = result[start + text_end + 2..].find(')') {
                let text = &result[start + 1..start + text_end];
                let url = &result[start + text_end + 2..start + text_end + 2 + url_end];
                let replacement = format!("<a href=\"{}\">{}</a>", url, text);
                result.replace_range(start..start + text_end + 2 + url_end + 1, &replacement);
            } else {
                break;
            }
        } else {
            break;
        }
    }

    result
}

/// Escape JSX special characters
fn escape_jsx(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('{', "&#123;")
        .replace('}', "&#125;")
}

/// Generate the MDX module wrapper
fn generate_mdx_module(jsx: &str, frontmatter: &HashMap<String, String>) -> String {
    let mut code = String::new();

    // Export frontmatter as named exports
    for (key, value) in frontmatter {
        code.push_str(&format!(
            "export const {} = \"{}\";\n",
            key,
            value.replace('"', "\\\"")
        ));
    }

    // Export frontmatter object
    code.push_str("export const frontmatter = ");
    let fm_json: Vec<String> = frontmatter
        .iter()
        .map(|(k, v)| format!("\"{}\": \"{}\"", k, v.replace('"', "\\\"")))
        .collect();
    code.push_str(&format!("{{ {} }};\n", fm_json.join(", ")));

    // Default export: render function
    code.push_str("export default function MDXContent() {\n");
    code.push_str("  return (\n    <>\n");
    code.push_str(jsx);
    code.push_str("    </>\n  );\n");
    code.push_str("}\n");

    code
}

// ─── Feature 32: GraphQL file loading ─────────────────────────────────

/// GraphQL document types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphQLOperation {
    Query,
    Mutation,
    Subscription,
    Fragment,
}

/// Parsed GraphQL document
#[derive(Debug, Clone, Default)]
pub struct GraphQLDocument {
    pub operations: Vec<GraphQLNamedOperation>,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct GraphQLNamedOperation {
    pub name: String,
    pub kind: GraphQLOperation,
    pub body: String,
}

impl GraphQLOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
            Self::Subscription => "subscription",
            Self::Fragment => "fragment",
        }
    }
}

/// Returns true when `keyword` at byte offset `pos` sits on a definition
/// boundary — i.e. it is a standalone keyword at the start of a line or block,
/// not part of an identifier (`myquery`, `queryBuilder`) and not inside a
/// `#` line comment.
fn is_at_definition_boundary(source: &str, keyword: &str, pos: usize) -> bool {
    // Check the character before the keyword
    let before_ok = pos == 0 || {
        let before = source.as_bytes()[pos - 1] as char;
        before == '\n' || before == '{' || before.is_whitespace()
    };

    // Check the character after the keyword
    let after_pos = pos + keyword.len();
    let after_ok = after_pos >= source.len() || {
        let after = source.as_bytes()[after_pos] as char;
        after.is_whitespace() || after == '{' || after == '('
    };

    if !(before_ok && after_ok) {
        return false;
    }

    // Reject matches inside a `#` line comment (a `#` earlier on the same line)
    let line_start = source[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    if source[line_start..pos].contains('#') {
        return false;
    }

    true
}

/// Parse a GraphQL document and extract operations
pub fn parse_graphql(source: &str) -> GraphQLDocument {
    let mut doc = GraphQLDocument {
        raw: source.to_string(),
        operations: Vec::new(),
    };

    for kind in [
        GraphQLOperation::Query,
        GraphQLOperation::Mutation,
        GraphQLOperation::Subscription,
        GraphQLOperation::Fragment,
    ] {
        let keyword = kind.as_str();
        let mut search_pos = 0;
        while let Some(pos) = source[search_pos..].find(keyword) {
            let abs_pos = search_pos + pos;

            // Skip matches that aren't standalone keywords at a definition
            // boundary (identifiers, comments, etc.)
            if !is_at_definition_boundary(source, keyword, abs_pos) {
                search_pos = abs_pos + keyword.len();
                continue;
            }

            let after = &source[abs_pos + keyword.len()..];

            // Extract operation name
            let name_start = after
                .find(|c: char| c.is_alphabetic() || c == '_')
                .unwrap_or(0);
            let rest = &after[name_start..];
            let name_end = rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            let name = rest[..name_end].to_string();

            if name.is_empty() {
                search_pos = abs_pos + keyword.len();
                continue;
            }

            // Extract body (up to matching closing brace)
            let body_rest = &rest[name_end..];
            if let Some(brace) = body_rest.find('{') {
                let mut depth = 1;
                let mut end = brace + 1;
                let bytes = body_rest.as_bytes();
                while end < bytes.len() && depth > 0 {
                    match bytes[end] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    end += 1;
                }
                let body = body_rest[..end].to_string();

                doc.operations.push(GraphQLNamedOperation {
                    name: name.clone(),
                    kind: kind.clone(),
                    body,
                });

                search_pos = abs_pos + keyword.len() + name_start + name_end + end;
            } else {
                search_pos = abs_pos + keyword.len();
            }
        }
    }

    doc
}

/// Generate a JS module from a GraphQL document with named exports and TypeScript types
pub fn graphql_to_module(source: &str) -> String {
    let doc = parse_graphql(source);
    let mut code = String::new();

    // Export each operation as a string constant
    for op in &doc.operations {
        code.push_str(&format!(
            "export const {} = `{} {} {}`;\n",
            op.name,
            op.kind.as_str(),
            op.name,
            op.body
        ));
    }

    // Export the raw document
    code.push_str(&format!(
        "export const document = `{}`;\n",
        source.replace('`', "\\`")
    ));

    // Default export
    code.push_str("export default document;\n");

    // TypeScript type declarations (as comments for now)
    code.push_str("\n/* TypeScript types:\n");
    for op in &doc.operations {
        code.push_str(&format!(
            "export type {}Data = unknown;\nexport type {}Vars = unknown;\n",
            to_pascal_case(&op.name),
            to_pascal_case(&op.name)
        ));
    }
    code.push_str("*/\n");

    code
}

/// Convert snake_case or camelCase to PascalCase
fn to_pascal_case(s: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_uppercase().next().unwrap_or(c));
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

// ─── Feature 33: YAML/CSV/TSV imports ─────────────────────────────────

/// Transform YAML to ES module with named exports using serde-saphyr for proper
/// parsing of nested structures, lists, anchors, and multi-line strings.
pub fn transform_yaml(source: &str) -> String {
    let parsed: serde_json::Value = match serde_saphyr::from_str(source) {
        Ok(v) => v,
        Err(_) => {
            // Fallback: if the YAML is invalid, produce empty default export
            return "export default {};".to_string();
        }
    };

    let mut code = String::new();

    // Generate named exports for top-level object keys with valid JS identifiers
    if let serde_json::Value::Object(ref map) = parsed {
        for (key, val) in map {
            if key
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                && !key.chars().next().map(|c| c.is_numeric()).unwrap_or(true)
            {
                code.push_str(&format!(
                    "export const {} = {};\n",
                    key,
                    serde_json::to_string(val).unwrap_or("null".to_string())
                ));
            }
        }
    }

    // Default export — the full parsed YAML as a JS object
    code.push_str(&format!(
        "export default {};",
        serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".to_string())
    ));

    code
}

/// Parse a single CSV line, honoring quoted fields.
/// Handles commas inside quotes and `""` escaped quotes.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                // Check for escaped quote
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => {
                in_quotes = true;
            }
            ',' if !in_quotes => {
                fields.push(field.trim().to_string());
                field.clear();
            }
            _ => field.push(c),
        }
    }
    fields.push(field.trim().to_string());
    fields
}

/// Transform CSV to ES module with named exports
pub fn transform_csv(source: &str) -> String {
    let rows: Vec<Vec<String>> = source
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_csv_line)
        .collect();

    if rows.is_empty() {
        return "export default [];".to_string();
    }

    let headers = &rows[0];
    let data_rows = &rows[1..];

    let mut code = String::new();

    // Export headers as array
    code.push_str(&format!(
        "export const columns = {};\n",
        serde_json::to_string(headers).unwrap_or("[]".to_string())
    ));

    // Export row count
    code.push_str(&format!("export const rowCount = {};\n", data_rows.len()));

    // Export data as array of objects
    let mut objects = Vec::new();
    for row in data_rows {
        let pairs: Vec<String> = headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let val = row.get(i).map(|s| s.as_str()).unwrap_or("");
                format!("\"{}\": \"{}\"", h, val.replace('"', "\\\""))
            })
            .collect();
        objects.push(format!("{{ {} }}", pairs.join(", ")));
    }
    code.push_str(&format!("export const rows = [{}];\n", objects.join(", ")));

    // Default export
    code.push_str("export default rows;\n");

    code
}

/// Transform TSV to ES module with named exports
pub fn transform_tsv(source: &str) -> String {
    let csv_source = source.replace('\t', ",");
    transform_csv(&csv_source)
}

// ─── Feature 34: Image format auto-selection ──────────────────────────

/// Determine the best image format based on browser support and file size
#[derive(Debug, Clone)]
pub struct ImageFormatDecision {
    /// Primary format to use
    pub primary: String,
    /// Fallback format
    pub fallback: String,
    /// Whether to generate multiple formats
    pub generate_multiple: bool,
}

/// Decide which image format(s) to generate based on browser targets and size
pub fn select_image_format(
    original_format: &str,
    file_size: u64,
    supports_webp: bool,
    supports_avif: bool,
) -> ImageFormatDecision {
    // AVIF has best compression but limited support
    if supports_avif && file_size > 50_000 {
        return ImageFormatDecision {
            primary: "avif".to_string(),
            fallback: if supports_webp {
                "webp".to_string()
            } else {
                "jpg".to_string()
            },
            generate_multiple: true,
        };
    }

    // WebP has good compression and wide support
    if supports_webp && file_size > 10_000 {
        return ImageFormatDecision {
            primary: "webp".to_string(),
            fallback: original_format.to_string(),
            generate_multiple: true,
        };
    }

    // Keep original format for small images
    ImageFormatDecision {
        primary: original_format.to_string(),
        fallback: original_format.to_string(),
        generate_multiple: false,
    }
}

/// Generate a <picture> element with multiple format sources
pub fn generate_picture_element(
    _src: &str,
    formats: &[(&str, &str)],
    alt: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> String {
    let mut html = String::from("<picture>\n");

    // Add <source> for each format (except the last one which is the fallback)
    for (format, url) in &formats[..formats.len().saturating_sub(1)] {
        html.push_str(&format!(
            "  <source srcset=\"{}\" type=\"image/{}\" />\n",
            url, format
        ));
    }

    // Fallback <img> with the last format
    if let Some((_, fallback_url)) = formats.last() {
        let mut img = format!("  <img src=\"{}\" alt=\"{}\"", fallback_url, alt);
        if let Some(w) = width {
            img.push_str(&format!(" width=\"{}\"", w));
        }
        if let Some(h) = height {
            img.push_str(&format!(" height=\"{}\"", h));
        }
        img.push_str(" loading=\"lazy\" />\n");
        html.push_str(&img);
    }

    html.push_str("</picture>");
    html
}

// ─── Feature 35: Audio/video asset handling ───────────────────────────

/// Audio file extensions
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "ogg", "aac", "flac", "m4a"];

/// Video file extensions
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "webm", "mov", "avi", "mkv"];

/// Check if a file is an audio file
pub fn is_audio_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| AUDIO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Check if a file is a video file
pub fn is_video_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|ext| VIDEO_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Transform audio asset to ES module with URL export
pub fn transform_audio_asset(file_path: &str, is_inline: bool, source: &[u8]) -> String {
    if is_inline {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(source);
        let mime = guess_mime(file_path);
        format!("export default \"data:{};base64,{}\";", mime, b64)
    } else {
        let url = format!("/{}", crate::normalize_path_str(file_path));
        format!("export default \"{}\";", url)
    }
}

/// Transform video asset to ES module with URL export
/// #78: Also exports poster frame URL for video files
pub fn transform_video_asset(file_path: &str, is_inline: bool, source: &[u8]) -> String {
    if is_inline {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(source);
        let mime = guess_mime(file_path);
        format!("export default \"data:{};base64,{}\";", mime, b64)
    } else {
        let url = format!("/{}", crate::normalize_path_str(file_path));
        // #78: Generate poster frame URL alongside video URL
        // Poster is extracted as first frame and saved as .jpg next to video
        let poster_url = format!(
            "/{}.poster.jpg",
            crate::normalize_path_str(file_path)
                .trim_end_matches(".mp4")
                .trim_end_matches(".webm")
                .trim_end_matches(".mov")
                .trim_end_matches(".avi")
                .trim_end_matches(".mkv")
        );
        format!(
            r#"export default "{}";
export const src = "{}";
export const poster = "{}";"#,
            url, url, poster_url
        )
    }
}

// ─── Feature 36: PDF asset handling ───────────────────────────────────

/// Transform PDF asset to ES module
/// Small PDFs can be inlined as base64, large ones get URL exports
pub fn transform_pdf_asset(file_path: &str, is_inline: bool, source: &[u8]) -> String {
    if is_inline {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(source);
        format!("export default \"data:application/pdf;base64,{}\";", b64)
    } else {
        let url = format!("/{}", crate::normalize_path_str(file_path));
        format!("export default \"{}\";", url)
    }
}

// ─── Feature 37: Asset manifest generation ────────────────────────────

/// Asset manifest entry mapping source path to hashed output path
#[derive(Debug, Clone, serde::Serialize)]
pub struct AssetManifestEntry {
    /// Original source path
    pub source: String,
    /// Hashed output path
    pub output: String,
    /// Content hash
    pub hash: String,
    /// File size in bytes
    pub size: u64,
    /// MIME type
    pub mime_type: String,
}

/// Asset manifest mapping all asset imports to their hashed output paths
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AssetManifest {
    /// Map of source path → manifest entry
    pub assets: HashMap<String, AssetManifestEntry>,
}

impl AssetManifest {
    /// Create a new empty manifest
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an asset to the manifest
    pub fn add(&mut self, source: &str, output: &str, hash: &str, size: u64, mime_type: &str) {
        self.assets.insert(
            source.to_string(),
            AssetManifestEntry {
                source: source.to_string(),
                output: output.to_string(),
                hash: hash.to_string(),
                size,
                mime_type: mime_type.to_string(),
            },
        );
    }

    /// Get an asset's output path by source path
    pub fn get(&self, source: &str) -> Option<&AssetManifestEntry> {
        self.assets.get(source)
    }

    /// Serialize the manifest to JSON
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Serialize the manifest to a simple mapping (source → output)
    pub fn to_simple_map(&self) -> String {
        let map: HashMap<&str, &str> = self
            .assets
            .iter()
            .map(|(k, v)| (k.as_str(), v.output.as_str()))
            .collect();
        serde_json::to_string_pretty(&map).unwrap_or_else(|_| "{}".to_string())
    }

    /// Merge another manifest into this one
    pub fn merge(&mut self, other: AssetManifest) {
        for (k, v) in other.assets {
            self.assets.insert(k, v);
        }
    }
}

/// Generate a content hash for an asset file
pub fn asset_hash(source: &[u8]) -> String {
    let hash = blake3::hash(source);
    hash.to_hex()[..12].to_string()
}

/// Generate a hashed output path for an asset
/// e.g., "images/logo.png" → "assets/logo-a1b2c3d4e5f6.png"
pub fn hashed_output_path(source_path: &str, hash: &str) -> String {
    let path = Path::new(source_path);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("asset");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.is_empty() {
        format!("assets/{}-{}", stem, hash)
    } else {
        format!("assets/{}-{}.{}", stem, hash, ext)
    }
}

// ─── Utility ──────────────────────────────────────────────────────────

/// Guess MIME type from file extension
fn guess_mime(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "pdf" => "application/pdf",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compile_mdx_simple() {
        let mdx = "# Hello World\n\nThis is **bold** and *italic*.";
        let result = compile_mdx(mdx, "test.mdx");
        assert!(result.code.contains("<h1>Hello World</h1>"));
        assert!(result.code.contains("<strong>bold</strong>"));
        assert!(result.code.contains("<em>italic</em>"));
        assert!(result.code.contains("MDXContent"));
    }

    #[test]
    fn test_mdx_with_frontmatter() {
        let mdx = "---\ntitle: My Post\nauthor: John\n---\n# Hello";
        let result = compile_mdx(mdx, "test.mdx");
        assert_eq!(
            result.frontmatter.get("title"),
            Some(&"My Post".to_string())
        );
        assert!(result.code.contains("export const title"));
    }

    #[test]
    fn test_parse_graphql() {
        let gql = "query GetUser { user { id name } }\nmutation UpdateUser { updateUser { id } }";
        let doc = parse_graphql(gql);
        assert_eq!(doc.operations.len(), 2);
        assert_eq!(doc.operations[0].name, "GetUser");
        assert_eq!(doc.operations[0].kind, GraphQLOperation::Query);
        assert_eq!(doc.operations[1].name, "UpdateUser");
        assert_eq!(doc.operations[1].kind, GraphQLOperation::Mutation);
    }

    #[test]
    fn test_graphql_to_module() {
        let gql = "query GetUser { user { id } }";
        let code = graphql_to_module(gql);
        assert!(code.contains("export const GetUser"));
        assert!(code.contains("export default"));
    }

    #[test]
    fn test_transform_yaml() {
        let yaml = "name: Test\nversion: 2\nenabled: true";
        let code = transform_yaml(yaml);
        assert!(code.contains("export const name = \"Test\""));
        assert!(code.contains("export const version = 2"));
        assert!(code.contains("export const enabled = true"));
    }

    #[test]
    fn test_transform_yaml_nested_anchors_and_lists() {
        let yaml = "base: &b
  a: 1
child:
  <<: *b
  list: [x, z]
multi: |
  line1
  line2
";
        let code = transform_yaml(yaml);
        assert!(code.contains("export const base = {\"a\":1}"), "{code}");
        assert!(code.contains("\"list\":[\"x\",\"z\"]"), "{code}");
        assert!(code.contains("line1\\nline2"), "{code}");
    }

    #[test]
    fn test_transform_yaml_invalid_falls_back_to_empty_default() {
        assert_eq!(transform_yaml("a: [unterminated"), "export default {};");
    }

    #[test]
    fn test_transform_csv() {
        let csv = "name,age\nAlice,30\nBob,25";
        let code = transform_csv(csv);
        assert!(code.contains("export const columns"));
        assert!(code.contains("export const rowCount = 2"));
        assert!(code.contains("export const rows"));
        assert!(code.contains("Alice"));
    }

    #[test]
    fn test_select_image_format_avif() {
        let decision = select_image_format("png", 100_000, true, true);
        assert_eq!(decision.primary, "avif");
        assert!(decision.generate_multiple);
    }

    #[test]
    fn test_select_image_format_webp() {
        let decision = select_image_format("png", 20_000, true, false);
        assert_eq!(decision.primary, "webp");
        assert!(decision.generate_multiple);
    }

    #[test]
    fn test_select_image_format_keep_original() {
        let decision = select_image_format("png", 5_000, true, true);
        assert_eq!(decision.primary, "png");
        assert!(!decision.generate_multiple);
    }

    #[test]
    fn test_is_audio_file() {
        assert!(is_audio_file("song.mp3"));
        assert!(is_audio_file("audio.wav"));
        assert!(!is_audio_file("video.mp4"));
    }

    #[test]
    fn test_is_video_file() {
        assert!(is_video_file("clip.mp4"));
        assert!(is_video_file("video.webm"));
        assert!(!is_video_file("song.mp3"));
    }

    #[test]
    fn test_asset_manifest() {
        let mut manifest = AssetManifest::new();
        manifest.add(
            "images/logo.png",
            "assets/logo-a1b2c3.png",
            "a1b2c3",
            5000,
            "image/png",
        );
        let json = manifest.to_json();
        assert!(json.contains("images/logo.png"));
        assert!(json.contains("assets/logo-a1b2c3.png"));
    }

    #[test]
    fn test_hashed_output_path() {
        let path = hashed_output_path("images/logo.png", "a1b2c3d4e5f6");
        assert_eq!(path, "assets/logo-a1b2c3d4e5f6.png");
    }
}
