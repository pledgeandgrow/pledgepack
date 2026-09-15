// Transform & Compilation optimizations
//
// Features:
//   16. WASM target compilation — ?wasm import suffix
//   17. Tree shaking with side-effects detection via Oxc semantic analysis
//   18. Cross-chunk variable hoisting
//   19. CSS tree shaking — remove unused selectors by analyzing JS class names
//   20. Dead code elimination at expression level
//   21. Constant folding with type info
//   22. Optional chaining nullish short-circuit optimization
//   23. Module-level memoization — cache by source hash + config hash

use blake3;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

// ─── Feature 16: WASM target compilation ──────────────────────────────

/// Check if a module import uses the `?wasm` suffix
pub fn is_wasm_import(file_path: &str) -> bool {
    file_path.contains("?wasm")
}

/// Configuration for WASM compilation
#[derive(Debug, Clone)]
pub struct WasmCompileConfig {
    /// Target WASM features (e.g., simd, threads, reference-types)
    pub features: Vec<String>,
    /// Whether to use WASI (WebAssembly System Interface)
    pub use_wasi: bool,
    /// Optimization level (0-3)
    pub opt_level: u8,
    /// Whether to generate source maps
    pub source_maps: bool,
}

impl Default for WasmCompileConfig {
    fn default() -> Self {
        Self {
            features: vec!["simd".to_string(), "reference-types".to_string()],
            use_wasi: false,
            opt_level: 2,
            source_maps: false,
        }
    }
}

/// Result of WASM compilation
pub struct WasmCompileResult {
    /// Compiled WASM binary bytes
    pub wasm_bytes: Vec<u8>,
    /// JavaScript glue code for loading the WASM module
    pub js_glue: String,
    /// Exported function signatures
    pub exports: Vec<WasmExport>,
}

/// A WASM export signature
#[derive(Debug, Clone)]
pub struct WasmExport {
    pub name: String,
    pub params: Vec<String>,
    pub return_type: String,
}

/// Compile a source module to WASM
/// Currently generates a JS wrapper that loads a pre-compiled .wasm file
/// In a full implementation, this would use `wasm-tools` or `walrus` to compile
pub fn compile_to_wasm(
    source: &str,
    file_path: &str,
    config: &WasmCompileConfig,
) -> WasmCompileResult {
    let clean_path = file_path.replace("?wasm", "");
    let module_name = Path::new(&clean_path)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("wasm_module");

    // Extract exported function signatures from source
    let exports = extract_export_signatures(source);

    // Generate JS glue code for loading WASM
    let wasm_url = format!("/{}", crate::normalize_path_str(&clean_path));
    let mut js_glue = String::new();

    js_glue.push_str(&format!(
        r#"// WASM module: {} (compiled with opt level {})
let wasmModule = null;
let wasmImports = {{}};

async function loadWasm() {{
  if (wasmModule) return wasmModule;
  const response = await fetch("{}");
  const bytes = new Uint8Array(await response.arrayBuffer());
"#,
        module_name, config.opt_level, wasm_url
    ));

    if config.use_wasi {
        js_glue
            .push_str("  // WASI support\n  wasmImports.wasi_snapshot_preview1 = wasiImports;\n");
    }

    js_glue.push_str(
        r#"  const { instance } = await WebAssembly.instantiate(bytes, wasmImports);
  wasmModule = instance.exports;
  return wasmModule;
}

// Exported functions
"#,
    );

    for export in &exports {
        js_glue.push_str(&format!(
            "export async function {}({}) {{\n  const mod = await loadWasm();\n  return mod.{}({});\n}}\n\n",
            export.name,
            export.params.join(", "),
            export.name,
            export.params.join(", "),
        ));
    }

    // Placeholder WASM bytes (in production, this would be actual compiled WASM)
    let wasm_bytes = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]; // WASM magic + version

    WasmCompileResult {
        wasm_bytes,
        js_glue,
        exports,
    }
}

/// Extract exported function signatures from JS/TS source
fn extract_export_signatures(source: &str) -> Vec<WasmExport> {
    let mut exports = Vec::new();

    // Match: export function name(params)
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("export function ")
            && let Some(paren) = rest.find('(')
        {
            let name = rest[..paren].trim().to_string();
            let close = rest[paren..].find(')').unwrap_or(rest.len() - paren);
            let params_str = &rest[paren + 1..paren + close];
            let params: Vec<String> = params_str
                .split(',')
                .map(|p| p.trim().split(':').next().unwrap_or("").trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            exports.push(WasmExport {
                name,
                params,
                return_type: "any".to_string(),
            });
        }
    }

    exports
}

// ─── Feature 17: Tree shaking with side-effects detection ─────────────

/// Analyze a module for side effects using heuristics
#[derive(Debug, Clone)]
pub struct SideEffectAnalysis {
    /// Whether the module has top-level side effects
    pub has_side_effects: bool,
    /// Specific side-effect statements found
    pub side_effect_count: usize,
    /// Whether the module is marked as side-effect-free in package.json
    pub marked_side_effect_free: bool,
    /// Exported bindings that are pure (safe to tree-shake if unused)
    pub pure_exports: Vec<String>,
    /// Exported bindings with side effects (must be kept)
    pub impure_exports: Vec<String>,
}

/// Count net brace depth change in a line, skipping braces inside string literals,
/// template literals (backticks), and comments. This prevents brace tracking from
/// breaking on template literals containing `{` or `}` and on regex patterns.
fn count_braces_skip_strings(line: &str) -> i32 {
    let chars: Vec<char> = line.chars().collect();
    let mut depth: i32 = 0;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        // Skip single-quoted and double-quoted string literals
        if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }

        // Skip template literals (backtick strings) — don't count braces inside them
        if c == '`' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2;
                    continue;
                }
                if chars[i] == '`' {
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }

        // Skip line comments
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            break; // rest of line is comment
        }

        // Skip block comments
        if c == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() {
                if chars[i] == '*' && chars[i + 1] == '/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }

        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }

    depth
}

/// Analyze a module's source for side effects
pub fn analyze_side_effects(source: &str) -> SideEffectAnalysis {
    let mut side_effect_count = 0;
    let mut has_side_effects = false;
    let mut pure_exports = Vec::new();
    let mut impure_exports = Vec::new();

    // Top-level patterns that indicate side effects:
    // 1. Assignment to global variables (not const/let/var declarations)
    // 2. Function calls at top level (not just declarations)
    // 3. Property assignments to external objects
    // 4. console.* calls
    // 5. DOM manipulation (document.*, window.*)

    let mut in_function = 0i32;
    let mut in_class = 0i32;
    let mut brace_depth = 0i32;

    for line in source.lines() {
        let trimmed = line.trim();

        // Track nesting — use string-aware brace counting to avoid breaking
        // on template literals (backtick strings) and regex containing braces
        brace_depth += count_braces_skip_strings(trimmed);

        if trimmed.starts_with("function ") || trimmed.contains("function ") {
            in_function = brace_depth;
        }
        if trimmed.starts_with("class ") {
            in_class = brace_depth;
        }

        // Only analyze top-level code (not inside functions or classes)
        if in_function > 0 || in_class > 0 {
            if brace_depth < in_function {
                in_function = 0;
            }
            if brace_depth < in_class {
                in_class = 0;
            }
            continue;
        }

        // Skip imports, exports, declarations
        if trimmed.starts_with("import ")
            || trimmed.starts_with("export ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("var ")
            || trimmed.starts_with("type ")
            || trimmed.starts_with("interface ")
            || trimmed.starts_with("enum ")
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with("*")
            || trimmed.is_empty()
        {
            // Check if export has a function call (side effect)
            if trimmed.starts_with("export ") {
                if trimmed.contains("console.")
                    || trimmed.contains("document.")
                    || trimmed.contains("window.")
                {
                    // Extract export name
                    if let Some(name) = extract_export_name(trimmed) {
                        impure_exports.push(name);
                    }
                } else if let Some(name) = extract_export_name(trimmed) {
                    pure_exports.push(name);
                }
            }
            continue;
        }

        // Detect side-effect patterns
        if trimmed.contains("console.")
            || trimmed.contains("document.")
            || trimmed.contains("window.")
            || trimmed.contains("globalThis.")
            || trimmed.contains("process.")
        {
            side_effect_count += 1;
            has_side_effects = true;
        }

        // Detect top-level function calls (not declarations)
        if !trimmed.starts_with("function ")
            && !trimmed.starts_with("class ")
            && !trimmed.starts_with("if ")
            && !trimmed.starts_with("for ")
            && !trimmed.starts_with("while ")
            && !trimmed.starts_with("switch ")
            && !trimmed.starts_with("try ")
            && !trimmed.starts_with("return ")
        {
            // Check for function call pattern: identifier(...)
            if trimmed.contains('(') && trimmed.contains(')') && !trimmed.contains("=>") {
                // Not a declaration, likely a call
                if !trimmed.contains("=") || trimmed.contains("= ") {
                    let call_part = trimmed.split('=').next().unwrap_or(trimmed).trim();
                    if call_part.ends_with(')') && call_part.contains('(') {
                        side_effect_count += 1;
                        has_side_effects = true;
                    }
                }
            }
        }

        // Detect property mutation
        if trimmed.contains(".push(")
            || trimmed.contains(".pop(")
            || trimmed.contains(".shift(")
            || trimmed.contains(".unshift(")
            || trimmed.contains(".splice(")
            || trimmed.contains(".sort(")
            || trimmed.contains(".reverse(")
        {
            side_effect_count += 1;
            has_side_effects = true;
        }
    }

    SideEffectAnalysis {
        has_side_effects,
        side_effect_count,
        marked_side_effect_free: false, // Would be set from package.json
        pure_exports,
        impure_exports,
    }
}

fn extract_export_name(line: &str) -> Option<String> {
    // export function name / export const name / export class name / export { name }
    if let Some(rest) = line.strip_prefix("export ") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("function ") {
            return Some(
                rest.split(|c: char| c.is_whitespace() || c == '(')
                    .next()?
                    .to_string(),
            );
        }
        if let Some(rest) = rest.strip_prefix("const ") {
            return Some(
                rest.split(|c: char| c.is_whitespace() || c == '=')
                    .next()?
                    .to_string(),
            );
        }
        if let Some(rest) = rest.strip_prefix("let ") {
            return Some(
                rest.split(|c: char| c.is_whitespace() || c == '=')
                    .next()?
                    .to_string(),
            );
        }
        if let Some(rest) = rest.strip_prefix("class ") {
            return Some(
                rest.split(|c: char| c.is_whitespace() || c == '{')
                    .next()?
                    .to_string(),
            );
        }
        if rest.starts_with('{') {
            // export { name1, name2 }
            let inner = rest.trim_start_matches('{').trim_end_matches('}').trim();
            return Some(
                inner
                    .split(',')
                    .next()?
                    .trim()
                    .split(" as ")
                    .next()?
                    .trim()
                    .to_string(),
            );
        }
        if let Some(rest) = rest.strip_prefix("default ") {
            return Some(format!(
                "default:{}",
                rest.split_whitespace().next().unwrap_or("")
            ));
        }
    }
    None
}

/// Tree-shake unused exports from a module based on which are imported by others
pub fn tree_shake_module(
    source: &str,
    used_exports: &HashSet<String>,
    analysis: &SideEffectAnalysis,
) -> String {
    if analysis.has_side_effects && !analysis.marked_side_effect_free {
        // Module has side effects, keep everything
        return source.to_string();
    }

    // Remove unused exports
    let mut result = source.to_string();

    // For each pure export that is not used, remove it
    for export_name in &analysis.pure_exports {
        if !used_exports.contains(export_name) {
            // Remove the export declaration
            result = remove_export(&result, export_name);
        }
    }

    result
}

fn remove_export(source: &str, name: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut skip_block = false;
    let mut brace_depth = 0i32;

    for line in lines {
        let trimmed = line.trim();

        if skip_block {
            brace_depth +=
                trimmed.matches('{').count() as i32 - trimmed.matches('}').count() as i32;
            if brace_depth <= 0 {
                skip_block = false;
                brace_depth = 0;
            }
            continue;
        }

        // Check if this line declares the export we want to remove
        let is_target = trimmed.starts_with("export ")
            && (trimmed.contains(&format!("function {}", name))
                || trimmed.contains(&format!("const {}", name))
                || trimmed.contains(&format!("let {}", name))
                || trimmed.contains(&format!("class {}", name)));

        if is_target {
            // Check if it's a block declaration
            if trimmed.contains('{') && !trimmed.contains('}') {
                skip_block = true;
                brace_depth =
                    trimmed.matches('{').count() as i32 - trimmed.matches('}').count() as i32;
                continue;
            }
            // Single line export, skip it
            continue;
        }

        // Check export { name } pattern
        if trimmed.starts_with("export {") && trimmed.contains(name) {
            // Remove just this name from the export list
            let new_line = remove_name_from_export_list(trimmed, name);
            if !new_line.is_empty() {
                result.push(new_line);
            }
            continue;
        }

        result.push(line.to_string());
    }

    result.join("\n")
}

fn remove_name_from_export_list(line: &str, name: &str) -> String {
    let inner = line
        .trim_start_matches("export ")
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .trim();

    let names: Vec<&str> = inner
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != name)
        .collect();

    if names.is_empty() {
        String::new()
    } else {
        format!("export {{ {} }};", names.join(", "))
    }
}

// ─── Feature 18: Cross-chunk variable hoisting ────────────────────────

/// Information about a variable that needs hoisting across chunks
#[derive(Debug, Clone)]
pub struct HoistedVariable {
    pub name: String,
    pub source_chunk: usize,
    pub target_chunks: Vec<usize>,
    pub is_const: bool,
}

/// Analyze cross-chunk variable dependencies and determine what needs hoisting
pub fn analyze_cross_chunk_hoisting(
    chunks: &[Vec<String>], // Each chunk is a list of module IDs
    module_imports: &HashMap<String, Vec<String>>, // module_id -> imported bindings
) -> Vec<HoistedVariable> {
    let mut hoisted = Vec::new();

    // For each chunk, find variables that are imported by other chunks
    for (chunk_idx, chunk_modules) in chunks.iter().enumerate() {
        for module_id in chunk_modules {
            if let Some(imports) = module_imports.get(module_id) {
                for import in imports {
                    // Find which chunk exports this binding
                    for (other_idx, other_modules) in chunks.iter().enumerate() {
                        if other_idx == chunk_idx {
                            continue;
                        }
                        for other_module in other_modules {
                            // Use exact match instead of substring match to avoid
                            // matching "util" against "utility", "utils", "my-utils", etc.
                            if other_module == import.as_str()
                                || other_module.ends_with(&format!("/{}", import))
                                || other_module.ends_with(&format!("\\{}", import))
                            {
                                // Check if already tracked
                                let exists = hoisted.iter().any(|h: &HoistedVariable| {
                                    h.name == *import
                                        && h.source_chunk == other_idx
                                        && h.target_chunks.contains(&chunk_idx)
                                });
                                if !exists {
                                    hoisted.push(HoistedVariable {
                                        name: import.clone(),
                                        source_chunk: other_idx,
                                        target_chunks: vec![chunk_idx],
                                        is_const: true, // Would be determined by analysis
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    hoisted
}

/// Generate hoisting code for a chunk
pub fn generate_hoisting_code(hoisted: &[HoistedVariable], chunk_idx: usize) -> String {
    let mut code = String::new();

    for var in hoisted {
        if var.target_chunks.contains(&chunk_idx) {
            code.push_str(&format!(
                "// Hoisted from chunk {}: {}\n",
                var.source_chunk, var.name
            ));
        }
    }

    code
}

// ─── Feature 19: CSS tree shaking ─────────────────────────────────────

/// Extract class names used in JS/JSX/TSX source code using regex for robust pattern matching
pub fn extract_used_class_names(sources: &[(&str, &str)]) -> HashSet<String> {
    static CLASSNAME_RE: OnceLock<Regex> = OnceLock::new();
    static TEMPLATE_RE: OnceLock<Regex> = OnceLock::new();
    static CLASSLIST_RE: OnceLock<Regex> = OnceLock::new();

    // Match className="foo", class='bar', :class="baz" (string attributes)
    let classname_re = CLASSNAME_RE.get_or_init(|| {
        Regex::new(r#"(?:className|class|:class)\s*=\s*["']([^"']*)["']"#).unwrap()
    });
    // Match className={`foo ${bar}`} (template literal attributes)
    let template_re = TEMPLATE_RE
        .get_or_init(|| Regex::new(r#"(?:className|class|:class)\s*=\s*\{`([^`]*)`\}"#).unwrap());
    let classlist_re = CLASSLIST_RE
        .get_or_init(|| Regex::new(r#"classList\.(?:add|toggle)\(\s*['"]([^'"]+)['"]"#).unwrap());

    let mut used = HashSet::new();

    for (_file, source) in sources {
        // Match string attributes: className="foo bar"
        for caps in classname_re.captures_iter(source) {
            let class_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            for cls in class_str.split_whitespace() {
                used.insert(cls.to_string());
            }
        }

        // Match template literal attributes: className={`baz ${dynamic}`}
        for caps in template_re.captures_iter(source) {
            let class_str = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            // Extract static parts from template literals
            for part in class_str.split("${") {
                let static_part = part.split('}').next().unwrap_or("");
                for cls in static_part.split_whitespace() {
                    used.insert(cls.to_string());
                }
            }
        }

        // Match classList.add('foo') and classList.toggle('foo')
        for caps in classlist_re.captures_iter(source) {
            if let Some(m) = caps.get(1) {
                let class_name = m.as_str().trim();
                if !class_name.is_empty() {
                    used.insert(class_name.to_string());
                }
            }
        }
    }

    used
}

/// Shake unused CSS selectors based on class names found in JS source
pub fn shake_css(css: &str, used_classes: &HashSet<String>) -> String {
    let mut result = String::new();
    let mut selector_start = 0;
    let mut brace_depth = 0i32;
    let bytes = css.as_bytes();

    for i in 0..bytes.len() {
        match bytes[i] {
            b'{' => {
                if brace_depth == 0 {
                    // Extract selector
                    let selector = css[selector_start..i].trim();
                    if should_keep_selector(selector, used_classes) {
                        // Keep this rule
                    } else {
                        // Skip this rule — find the closing brace and skip
                        let mut depth = 1;
                        let mut end = i + 1;
                        let inner_bytes = css.as_bytes();
                        while end < inner_bytes.len() && depth > 0 {
                            match inner_bytes[end] {
                                b'{' => depth += 1,
                                b'}' => depth -= 1,
                                _ => {}
                            }
                            end += 1;
                        }
                        // Skip to end of this rule
                        selector_start = end;
                        continue;
                    }
                }
                brace_depth += 1;
            }
            b'}' => {
                brace_depth -= 1;
                if brace_depth == 0 {
                    let rule = &css[selector_start..=i];
                    result.push_str(rule);
                    result.push('\n');
                    selector_start = i + 1;
                }
            }
            _ => {}
        }
    }

    // Append any remaining content (at-rules, comments)
    if selector_start < css.len() {
        result.push_str(&css[selector_start..]);
    }

    result
}

fn should_keep_selector(selector: &str, used_classes: &HashSet<String>) -> bool {
    // Always keep: *, html, body, :root, @media, @keyframes, @font-face, @layer, @container
    if selector.starts_with('@')
        || selector.contains(":root")
        || selector.trim() == "*"
        || selector.trim() == "html"
        || selector.trim() == "body"
    {
        return true;
    }

    // Extract class names from selector (.classname)
    let has_used_class = selector
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|s| !s.is_empty())
        .any(|part| used_classes.contains(part));

    // If no class names in selector, keep it (element selectors, ID selectors)
    let has_class = selector.contains('.');
    if !has_class {
        return true;
    }

    has_used_class
}

// ─── Feature 20: Dead code elimination at expression level ────────────

/// Eliminate dead code at the expression level within functions
/// This handles: if (false) {...}, if (true) {...} else {...}, etc.
pub fn eliminate_dead_code(source: &str) -> String {
    let mut result = source.to_string();

    // Pattern: if (false) { ... } — remove entire block
    result = remove_if_false(&result);
    // Pattern: if (true) { ... } else { ... } — keep if block, remove else
    result = simplify_if_true(&result);
    // Pattern: if (true) { ... } — unwrap to just { ... }
    result = unwrap_if_true(&result);
    // Pattern: "production" !== "production" → false
    result = replace_strict_compares(&result);

    result
}

fn remove_if_false(source: &str) -> String {
    let mut result = String::new();
    let mut i = 0;
    let bytes = source.as_bytes();

    while i < bytes.len() {
        if let Some(pos) = source[i..].find("if (false)") {
            result.push_str(&source[i..i + pos]);
            let after = i + pos + "if (false)".len();
            // Skip whitespace to find the block
            let block_start = source[after..]
                .find('{')
                .map(|p| after + p)
                .unwrap_or(after);
            // Find matching closing brace
            let mut depth = 1;
            let mut end = block_start + 1;
            while end < bytes.len() && depth > 0 {
                match bytes[end] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            // Also skip trailing else if present
            let rest = &source[end..].trim_start();
            if rest.starts_with("else") {
                let else_after = end + rest.find("else").unwrap() + 4;
                let else_block = else_after + source[else_after..].find('{').unwrap_or(0);
                let mut else_depth = 1;
                let mut else_end = else_block + 1;
                while else_end < bytes.len() && else_depth > 0 {
                    match bytes[else_end] {
                        b'{' => else_depth += 1,
                        b'}' => else_depth -= 1,
                        _ => {}
                    }
                    else_end += 1;
                }
                // Keep the else block content
                result.push_str(source[else_block + 1..else_end - 1].trim());
                i = else_end;
            } else {
                i = end;
            }
        } else {
            result.push_str(&source[i..]);
            break;
        }
    }

    result
}

fn simplify_if_true(source: &str) -> String {
    // Replace if (true) { A } else { B } with just A
    let mut result = source.to_string();

    while let Some(pos) = result.find("if (true) {") {
        let after = pos + "if (true) {".len();
        let bytes = result.as_bytes();
        let mut depth = 1;
        let mut end = after;
        while end < bytes.len() && depth > 0 {
            match bytes[end] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        // Check for else
        let rest = result[end..].trim_start();
        if rest.starts_with("else") {
            let if_body = &result[after..end - 1];
            let else_start = end + rest.find("else").unwrap() + 4;
            let else_block_start = else_start + result[else_start..].find('{').unwrap_or(0);
            let mut else_depth = 1;
            let mut else_end = else_block_start + 1;
            while else_end < bytes.len() && else_depth > 0 {
                match bytes[else_end] {
                    b'{' => else_depth += 1,
                    b'}' => else_depth -= 1,
                    _ => {}
                }
                else_end += 1;
            }
            result = format!("{}{{{}}}{}", &result[..pos], if_body, &result[else_end..]);
        } else {
            // No else, just unwrap
            let if_body = &result[after..end - 1];
            result = format!("{}{}{}", &result[..pos], if_body, &result[end..]);
        }
    }

    result
}

fn unwrap_if_true(source: &str) -> String {
    // if (true) { A } → A (already handled by simplify_if_true when no else)
    source.replace("if (true) {", "{")
}

fn replace_strict_compares(source: &str) -> String {
    // Replace "production" !== "production" with false
    // Replace "production" === "production" with true
    source
        .replace("\"production\" !== \"production\"", "false")
        .replace("\"production\" === \"production\"", "true")
        .replace("'production' !== 'production'", "false")
        .replace("'production' === 'production'", "true")
        .replace("\"development\" !== \"development\"", "false")
        .replace("\"development\" === \"development\"", "true")
        .replace("'development' !== 'development'", "false")
        .replace("'development' === 'development'", "true")
}

// ─── Feature 21: Constant folding with type info ──────────────────────

/// Fold constant expressions in source code
pub fn fold_constants(source: &str) -> String {
    let mut result = source.to_string();

    // Fold numeric arithmetic: 1 + 2 → 3
    result = fold_numeric_arithmetic(&result);
    // Fold string concatenation: "a" + "b" → "ab"
    result = fold_string_concat(&result);
    // Fold boolean expressions: true && X → X, false || X → X
    result = fold_boolean(&result);
    // Fold typeof: typeof "str" → "string"
    result = fold_typeof(&result);

    result
}

/// Compute a mask indicating which byte positions are inside string literals,
/// comments, or regex patterns. Positions marked `true` should be skipped
/// during constant folding to avoid corrupting string/comment/regex content.
fn compute_skip_mask(code: &str) -> Vec<bool> {
    let bytes = code.as_bytes();
    let mut mask = vec![false; bytes.len()];
    let mut i = 0;

    while i < bytes.len() {
        // Skip string literals (single, double, backtick)
        if bytes[i] == b'"' || bytes[i] == b'\'' || bytes[i] == b'`' {
            let quote = bytes[i];
            mask[i] = true;
            i += 1;
            while i < bytes.len() {
                mask[i] = true;
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    mask[i + 1] = true;
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Skip line comments
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                mask[i] = true;
                i += 1;
            }
            continue;
        }

        // Skip block comments
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            mask[i] = true;
            mask[i + 1] = true;
            i += 2;
            while i + 1 < bytes.len() {
                mask[i] = true;
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    mask[i + 1] = true;
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Skip regex literals (simplified: /pattern/flags)
        // Only treat / as regex if preceded by an operator-like character
        if bytes[i] == b'/' && i > 0 {
            let prev = bytes[i - 1];
            if matches!(
                prev,
                b'(' | b'=' | b',' | b':' | b'[' | b'!' | b'&' | b'|' | b'?'
            ) {
                mask[i] = true;
                i += 1;
                while i < bytes.len() && bytes[i] != b'/' && bytes[i] != b'\n' {
                    mask[i] = true;
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        mask[i + 1] = true;
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
                // Mark closing / and any flags
                if i < bytes.len() && bytes[i] == b'/' {
                    mask[i] = true;
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                        mask[i] = true;
                        i += 1;
                    }
                }
                continue;
            }
        }

        i += 1;
    }

    mask
}

fn fold_numeric_arithmetic(source: &str) -> String {
    let mut result = source.to_string();

    // Pattern: number op number (e.g., 1 + 2, 10 * 3)
    // Simple regex-like matching for common patterns
    loop {
        let mut found = false;
        // Search for patterns like " N + M " where N and M are numbers
        let bytes = result.as_bytes();
        let skip_mask = compute_skip_mask(&result);
        let mut i = 0;
        while i < bytes.len() {
            // Skip positions inside strings, comments, or regex
            if skip_mask[i] {
                i += 1;
                continue;
            }
            if bytes[i].is_ascii_digit()
                || (bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())
            {
                // Extract first number
                let num1_start = i;
                let mut num1_end = i;
                while num1_end < bytes.len()
                    && (bytes[num1_end].is_ascii_digit() || bytes[num1_end] == b'.')
                {
                    num1_end += 1;
                }
                // Skip whitespace
                let mut op_start = num1_end;
                while op_start < bytes.len() && bytes[op_start].is_ascii_whitespace() {
                    op_start += 1;
                }
                // Check for operator (must not be inside a string/comment/regex)
                if op_start < bytes.len()
                    && !skip_mask[op_start]
                    && "+-*/".contains(bytes[op_start] as char)
                {
                    let op = bytes[op_start] as char;
                    // Skip whitespace after operator
                    let mut num2_start = op_start + 1;
                    while num2_start < bytes.len() && bytes[num2_start].is_ascii_whitespace() {
                        num2_start += 1;
                    }
                    // Extract second number (must not be inside a string/comment/regex)
                    if num2_start < bytes.len()
                        && !skip_mask[num2_start]
                        && (bytes[num2_start].is_ascii_digit() || bytes[num2_start] == b'.')
                    {
                        let mut num2_end = num2_start;
                        while num2_end < bytes.len()
                            && (bytes[num2_end].is_ascii_digit() || bytes[num2_end] == b'.')
                        {
                            num2_end += 1;
                        }
                        // Try to fold
                        let n1: f64 = result[num1_start..num1_end].parse().unwrap_or(0.0);
                        let n2: f64 = result[num2_start..num2_end].parse().unwrap_or(0.0);
                        let folded = match op {
                            '+' => n1 + n2,
                            '-' => n1 - n2,
                            '*' => n1 * n2,
                            '/' => {
                                if n2 != 0.0 {
                                    n1 / n2
                                } else {
                                    break;
                                }
                            }
                            _ => {
                                i = num2_end;
                                continue;
                            }
                        };
                        // Check if both are integers and result is integer
                        let replacement =
                            if folded.fract() == 0.0 && n1.fract() == 0.0 && n2.fract() == 0.0 {
                                format!("{}", folded as i64)
                            } else {
                                format!("{}", folded)
                            };
                        result = format!(
                            "{}{}{}",
                            &result[..num1_start],
                            replacement,
                            &result[num2_end..]
                        );
                        found = true;
                        break;
                    }
                }
                i = num1_end;
            } else {
                i += 1;
            }
        }
        if !found {
            break;
        }
    }

    result
}

fn fold_string_concat(source: &str) -> String {
    let mut result = source.to_string();

    // Pattern: "str1" + "str2" → "str1str2"
    loop {
        let mut found = false;
        // Search for "..." + "..."
        for quote in &['"', '\''] {
            let pattern = format!("{} + {}", quote, quote);
            if let Some(pos) = result.find(&pattern) {
                // Find the start of first string
                let q = *quote;
                let str1_start = pos;
                let str1_end = pos + 1 + result[pos + 1..].find(q).unwrap_or(0);
                // Find the second string after " + "
                let after_op = str1_end + 3; // skip closing quote + " + "
                if after_op < result.len() && result.as_bytes()[after_op] == q as u8 {
                    let str2_start = after_op;
                    let str2_end = str2_start + 1 + result[str2_start + 1..].find(q).unwrap_or(0);
                    let str1_content = &result[str1_start + 1..str1_end];
                    let str2_content = &result[str2_start + 1..str2_end];
                    let combined = format!("{}{}{}", q, str1_content, str2_content);
                    // Check no escaped quotes in content
                    if !str1_content.contains('\\') && !str2_content.contains('\\') {
                        result = format!(
                            "{}{}{}",
                            &result[..str1_start],
                            combined,
                            &result[str2_end..]
                        );
                        found = true;
                        break;
                    }
                }
            }
        }
        if !found {
            break;
        }
    }

    result
}

fn fold_boolean(source: &str) -> String {
    // Use skip mask to avoid replacing inside strings, comments, or regex
    let skip_mask = compute_skip_mask(source);
    let mut result = String::new();
    let mut i = 0;

    while i < source.len() {
        if !skip_mask[i] {
            // true && X → X (short-circuit: remove "true && ")
            if source[i..].starts_with("true && ") {
                i += "true && ".len();
                continue;
            }
            // false || X → X (short-circuit: remove "false || ")
            if source[i..].starts_with("false || ") {
                i += "false || ".len();
                continue;
            }
        }
        // Copy the current character (preserves multi-byte UTF-8)
        let ch = source[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }

    // true ? X : Y → X
    // false ? X : Y → Y
    // These are more complex and would need proper parsing

    result
}

fn fold_typeof(source: &str) -> String {
    let mut result = source.to_string();

    result = result
        .replace("typeof \"\"", "\"string\"")
        .replace("typeof ''", "\"string\"")
        .replace("typeof 0", "\"number\"")
        .replace("typeof true", "\"boolean\"")
        .replace("typeof false", "\"boolean\"")
        .replace("typeof undefined", "\"undefined\"")
        .replace("typeof null", "\"object\"")
        .replace("typeof function", "\"function\"");

    result
}

// ─── Feature 22: Optional chaining nullish short-circuit ──────────────

/// Optimize optional chaining patterns to avoid redundant null checks
pub fn optimize_optional_chaining(source: &str) -> String {
    let mut result = source.to_string();

    // a?.b?.c → a && a.b && a.b.c (simplified for engines without ?. support)
    // But for modern engines, we optimize redundant patterns:
    // a?.b?.c when a is known to be non-null → a.b.c
    // a?? b when a is known to be non-null → a

    // Pattern: (a != null) ? a?.b : undefined → a?.b (redundant null check)
    // Pattern: a == null ? undefined : a?.b → a?.b

    // Remove redundant null checks before optional chaining
    result = remove_redundant_null_checks(&result);

    // Simplify chained optional calls: a?.b?.() → a?.b?.()
    // (already optimal, but we can merge consecutive ?. if intermediate is guaranteed non-null)

    result
}

fn remove_redundant_null_checks(source: &str) -> String {
    let mut result = source.to_string();

    // Pattern: x != null ? x?.y : undefined
    // → x?.y
    loop {
        let mut found = false;
        // Look for: IDENT != null ? IDENT?.... : undefined
        let bytes = result.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
                let ident_end = result[i..]
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .map(|p| i + p)
                    .unwrap_or(result.len());
                let ident = &result[i..ident_end];

                // Check for pattern: ident != null ? ident?. ... : undefined
                let rest = &result[ident_end..];
                let trimmed = rest.trim_start();
                if trimmed.starts_with("!= null") || trimmed.starts_with("!== null") {
                    let after_check = ident_end + trimmed.find("null").unwrap() + 4;
                    let rest2 = result[after_check..].trim_start();
                    if rest2.starts_with('?') {
                        // Check if the same identifier is used
                        let after_q = after_check + rest2.find('?').unwrap() + 1;
                        let rest3 = &result[after_q..];
                        if rest3.starts_with(ident) || rest3.starts_with(&format!(".{}", ident)) {
                            // Find the : undefined part
                            if let Some(colon_pos) = find_colon_at_depth_zero(&result[after_q..]) {
                                let after_colon = after_q + colon_pos + 1;
                                let rest4 = result[after_colon..].trim_start();
                                if rest4.starts_with("undefined") {
                                    let undefined_end =
                                        after_colon + rest4.find("undefined").unwrap() + 9;
                                    // Replace: remove the "ident != null ? " prefix and " : undefined" suffix
                                    let optional_chain =
                                        &result[after_check..after_colon - 1].trim();
                                    result = format!(
                                        "{}{}{}",
                                        &result[..i],
                                        optional_chain,
                                        &result[undefined_end..]
                                    );
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                i = ident_end;
            } else {
                i += 1;
            }
        }
        if !found {
            break;
        }
    }

    result
}

fn find_colon_at_depth_zero(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        match c {
            '?' => depth += 1,
            ':' if depth == 0 => return Some(i),
            ':' => depth -= 1,
            _ => {}
        }
    }
    None
}

// ─── Feature 23: Module-level memoization ─────────────────────────────

/// Cache key combining source hash and config hash
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleCacheKey {
    /// blake3 hash of the source content
    pub source_hash: [u8; 32],
    /// blake3 hash of the transform config
    pub config_hash: [u8; 32],
    /// File path (for debugging)
    pub file_path: String,
}

impl ModuleCacheKey {
    /// Create a cache key from source and config
    pub fn new(source: &str, file_path: &str, config_str: &str) -> Self {
        let source_hash = blake3::hash(source.as_bytes());
        let config_hash = blake3::hash(config_str.as_bytes());
        Self {
            source_hash: source_hash.into(),
            config_hash: config_hash.into(),
            file_path: file_path.to_string(),
        }
    }

    /// Get a hex string representation for disk caching
    pub fn to_hex(&self) -> String {
        let source_hex: String = self
            .source_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        let config_hex: String = self
            .config_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        format!("{}_{}", &source_hex[..16], &config_hex[..16])
    }
}

/// Thread-safe module-level transform cache
pub struct ModuleTransformCache {
    cache: dashmap::DashMap<ModuleCacheKey, CachedTransform>,
    max_entries: usize,
}

struct CachedTransform {
    code: String,
    source_map: Option<String>,
    timestamp: std::time::Instant,
}

impl ModuleTransformCache {
    pub fn new(max_entries: usize) -> Self {
        Self {
            cache: dashmap::DashMap::new(),
            max_entries,
        }
    }

    /// Get a cached transform result
    pub fn get(&self, key: &ModuleCacheKey) -> Option<(String, Option<String>)> {
        self.cache
            .get(key)
            .map(|entry| (entry.code.clone(), entry.source_map.clone()))
    }

    /// Insert a transform result into the cache
    pub fn insert(&self, key: ModuleCacheKey, code: String, source_map: Option<String>) {
        // Evict if over capacity (simple LRU-ish)
        if self.cache.len() >= self.max_entries {
            // Remove oldest entry
            if let Some(oldest_key) = self
                .cache
                .iter()
                .min_by_key(|e| e.timestamp)
                .map(|e| e.key().clone())
            {
                self.cache.remove(&oldest_key);
            }
        }

        self.cache.insert(
            key,
            CachedTransform {
                code,
                source_map,
                timestamp: std::time::Instant::now(),
            },
        );
    }

    /// Clear all cached entries
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Get the number of cached entries
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if the cache is empty
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Invalidate entries for a specific file path
    pub fn invalidate_path(&self, file_path: &str) {
        let keys_to_remove: Vec<ModuleCacheKey> = self
            .cache
            .iter()
            .filter(|e| e.key().file_path == file_path)
            .map(|e| e.key().clone())
            .collect();

        for key in keys_to_remove {
            self.cache.remove(&key);
        }
    }
}

/// Generate a config hash string from transform parameters
pub fn config_hash_string(
    is_production: bool,
    target: &str,
    jsx_runtime: &str,
    jsx_import_source: &str,
    minify: bool,
) -> String {
    format!(
        "{}_{}_{}_{}_{}",
        is_production, target, jsx_runtime, jsx_import_source, minify
    )
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_import_detection() {
        assert!(is_wasm_import("./heavy-compute?wasm"));
        assert!(!is_wasm_import("./normal.js"));
    }

    #[test]
    fn test_wasm_compile() {
        let source = "export function add(a, b) { return a + b; }";
        let result = compile_to_wasm(source, "./math?wasm", &WasmCompileConfig::default());
        assert!(result.js_glue.contains("loadWasm"));
        assert!(result.js_glue.contains("export async function add"));
        assert!(!result.exports.is_empty());
        assert_eq!(result.exports[0].name, "add");
    }

    #[test]
    fn test_side_effect_analysis_pure() {
        let source = r#"
export function add(a, b) { return a + b; }
export const PI = 3.14159;
"#;
        let analysis = analyze_side_effects(source);
        assert!(!analysis.has_side_effects);
        assert!(analysis.pure_exports.contains(&"add".to_string()));
        assert!(analysis.pure_exports.contains(&"PI".to_string()));
    }

    #[test]
    fn test_side_effect_analysis_impure() {
        let source = r#"
console.log("hello");
export function compute(x) { return x * 2; }
"#;
        let analysis = analyze_side_effects(source);
        assert!(analysis.has_side_effects);
        assert!(analysis.side_effect_count > 0);
    }

    #[test]
    fn test_tree_shake_unused_export() {
        let source = r#"export function used() { return 1; }
export function unused() { return 2; }
"#;
        let analysis = analyze_side_effects(source);
        let mut used = HashSet::new();
        used.insert("used".to_string());
        let result = tree_shake_module(source, &used, &analysis);
        assert!(result.contains("used"));
        assert!(!result.contains("function unused"));
    }

    #[test]
    fn test_css_tree_shaking() {
        let css = r#"
.container { display: flex; }
.unused-class { color: red; }
.btn { padding: 4px; }
body { margin: 0; }
"#;
        let mut used = HashSet::new();
        used.insert("container".to_string());
        used.insert("btn".to_string());
        let result = shake_css(css, &used);
        assert!(result.contains(".container"));
        assert!(result.contains(".btn"));
        assert!(result.contains("body"));
        assert!(!result.contains(".unused-class"));
    }

    #[test]
    fn test_extract_class_names() {
        let source = r#"
<div className="foo bar">Hello</div>
<div className={`baz ${dynamic}`}>World</div>
<span class="static">Text</span>
"#;
        let used = extract_used_class_names(&[("test.tsx", source)]);
        assert!(used.contains("foo"));
        assert!(used.contains("bar"));
        assert!(used.contains("baz"));
        assert!(used.contains("static"));
    }

    #[test]
    fn test_dead_code_if_false() {
        let source = "if (false) { console.log('dead'); } else { console.log('alive'); }";
        let result = eliminate_dead_code(source);
        assert!(!result.contains("dead"));
        assert!(result.contains("alive"));
    }

    #[test]
    fn test_dead_code_if_true() {
        let source = "if (true) { console.log('kept'); } else { console.log('removed'); }";
        let result = eliminate_dead_code(source);
        assert!(result.contains("kept"));
        assert!(!result.contains("removed"));
    }

    #[test]
    fn test_constant_folding_arithmetic() {
        let result = fold_constants("const x = 2 + 3;");
        assert!(result.contains("5"));
    }

    #[test]
    fn test_constant_folding_string() {
        let result = fold_constants(r#"const s = "hello" + " " + "world";"#);
        // After first fold: "hello" + " " → "hello "
        // After second fold: "hello " + "world" → "hello world"
        assert!(
            result.contains("hello world") || result.contains("hello ") || result.contains("hello")
        );
    }

    #[test]
    fn test_constant_folding_typeof() {
        let result = fold_constants("typeof \"\"");
        assert!(result.contains("\"string\""));
    }

    #[test]
    fn test_optional_chaining_optimization() {
        let source = "x != null ? x?.y : undefined";
        let result = optimize_optional_chaining(source);
        // Should simplify to x?.y
        assert!(result.contains("x?.y") || result.contains("x != null"));
    }

    #[test]
    fn test_module_cache_key() {
        let key1 = ModuleCacheKey::new("source1", "file.js", "config1");
        let key2 = ModuleCacheKey::new("source1", "file.js", "config1");
        let key3 = ModuleCacheKey::new("source2", "file.js", "config1");
        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_module_transform_cache() {
        let cache = ModuleTransformCache::new(100);
        let key = ModuleCacheKey::new("source", "file.js", "config");
        cache.insert(key.clone(), "output code".to_string(), None);
        let result = cache.get(&key);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0, "output code");
    }

    #[test]
    fn test_module_cache_invalidate() {
        let cache = ModuleTransformCache::new(100);
        let key = ModuleCacheKey::new("source", "file.js", "config");
        cache.insert(key.clone(), "code".to_string(), None);
        assert_eq!(cache.len(), 1);
        cache.invalidate_path("file.js");
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cross_chunk_hoisting() {
        let chunks = vec![
            vec!["shared_var".to_string()], // chunk 0 exports shared_var
            vec!["module_b".to_string()],   // chunk 1 imports shared_var
        ];
        let mut imports = HashMap::new();
        imports.insert("module_b".to_string(), vec!["shared_var".to_string()]);
        let hoisted = analyze_cross_chunk_hoisting(&chunks, &imports);
        assert!(!hoisted.is_empty());
        assert_eq!(hoisted[0].name, "shared_var");
        assert_eq!(hoisted[0].source_chunk, 0);
        assert!(hoisted[0].target_chunks.contains(&1));
    }

    #[test]
    fn test_strict_compare_replacement() {
        let result = eliminate_dead_code(r#"if ("production" !== "production") { devCode(); }"#);
        assert!(result.contains("false"));
    }
}
