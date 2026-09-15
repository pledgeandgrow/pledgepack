// Environment variable replacement, define, import.meta.glob expansion

use crate::config::PledgeConfig;
use crate::env::EnvVars;
use globset::Glob;
use std::path::Path;

/// Replace import.meta.env.* with actual environment variable values from .env files
pub(super) fn replace_env_vars(code: &str, config: &PledgeConfig) -> String {
    if !code.contains("import.meta.env") {
        return code.to_string();
    }

    let mode = if config.mode == crate::config::BuildMode::Production {
        crate::config::BuildMode::Production
    } else {
        crate::config::BuildMode::Development
    };

    let env = EnvVars::load(&config.root, mode, &config.env_prefix);
    env.inject_into_code(code, &config.env_prefix)
}

/// Inline process.env.* variables at build time (#51).
/// Replaces process.env.NODE_ENV with "production" or "development",
/// and inlines other process.env.* variables from the actual environment.
/// Also eliminates dead branches that become unreachable after inlining
/// (e.g., `if (process.env.NODE_ENV !== "production") { ... }` in production).
pub(super) fn inline_process_env(code: &str, is_production: bool) -> String {
    let mut result = code.to_string();

    let node_env = if is_production {
        "\"production\""
    } else {
        "\"development\""
    };
    result = result.replace("process.env.NODE_ENV", node_env);

    let mut env_vars_to_replace: Vec<(String, String)> = Vec::new();
    let mut search_pos = 0;
    while let Some(pos) = result[search_pos..].find("process.env.") {
        let abs_pos = search_pos + pos;
        let after = &result[abs_pos + "process.env.".len()..];
        let var_name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !var_name.is_empty() && var_name != "NODE_ENV" {
            let pattern = format!("process.env.{}", var_name);
            if !env_vars_to_replace.iter().any(|(p, _)| p == &pattern)
                && let Ok(value) = std::env::var(&var_name)
            {
                env_vars_to_replace.push((pattern, value));
            }
        }
        search_pos = abs_pos + "process.env.".len();
    }

    for (pattern, value) in env_vars_to_replace {
        let replacement = if value == "true" || value == "false" || value.parse::<f64>().is_ok() {
            value.clone()
        } else {
            format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
        };
        result = result.replace(&pattern, &replacement);
    }

    result = eliminate_dead_branches(&result);

    result
}

/// Eliminate dead branches that result from env var inlining.
/// Handles simple if-statements with constant conditions.
fn eliminate_dead_branches(code: &str) -> String {
    let mut result = code.to_string();

    let str_cmp_patterns: Vec<(&str, bool)> = vec![
        ("\"production\" === \"production\"", true),
        ("\"production\" !== \"production\"", false),
        ("\"development\" === \"development\"", true),
        ("\"development\" !== \"development\"", false),
        ("\"production\" == \"production\"", true),
        ("\"production\" != \"production\"", false),
    ];

    for (pattern, is_true) in &str_cmp_patterns {
        let search = format!("if ({})", pattern);
        while let Some(pos) = result.find(&search) {
            if let Some((block_start, block_end)) = find_block_after(&result, pos + search.len()) {
                let after = &result[block_end..];
                if after.trim_start().starts_with("else") {
                    let else_start = block_end + after.find("else").unwrap();
                    if *is_true {
                        if let Some((_, else_be)) = find_block_after(&result, else_start + 4) {
                            let if_content = result[block_start + 1..block_end].to_string();
                            result.replace_range(pos..else_be + 1, if_content.trim());
                            continue;
                        }
                    } else {
                        if let Some((else_bs, else_be)) = find_block_after(&result, else_start + 4)
                        {
                            let else_content = result[else_bs + 1..else_be].to_string();
                            result.replace_range(pos..else_be + 1, else_content.trim());
                            continue;
                        }
                    }
                }
                if *is_true {
                    let if_content = result[block_start + 1..block_end].to_string();
                    result.replace_range(pos..block_end + 1, if_content.trim());
                } else {
                    result.replace_range(pos..block_end + 1, "");
                }
            } else {
                break;
            }
        }
    }

    while let Some(pos) = result.find("if (false)") {
        if let Some((_block_start, block_end)) = find_block_after(&result, pos + "if (false)".len())
        {
            let after = &result[block_end..];
            if after.trim_start().starts_with("else") {
                let else_start = block_end + after.find("else").unwrap();
                let _after_else = &result[else_start + 4..];
                if let Some((else_bs, else_be)) = find_block_after(&result, else_start + 4) {
                    let else_content = result[else_bs + 1..else_be].to_string();
                    result.replace_range(pos..else_be + 1, else_content.trim());
                    continue;
                }
            }
            result.replace_range(pos..block_end + 1, "");
        } else {
            break;
        }
    }

    while let Some(pos) = result.find("if (true)") {
        if let Some((block_start, block_end)) = find_block_after(&result, pos + "if (true)".len()) {
            let after = &result[block_end..];
            if after.trim_start().starts_with("else") {
                let else_start = block_end + after.find("else").unwrap();
                if let Some((_, else_be)) = find_block_after(&result, else_start + 4) {
                    let if_content = result[block_start + 1..block_end].to_string();
                    result.replace_range(pos..else_be + 1, if_content.trim());
                    continue;
                }
            }
            let if_content = result[block_start + 1..block_end].to_string();
            result.replace_range(pos..block_end + 1, if_content.trim());
        } else {
            break;
        }
    }

    result
}

/// Find the { ... } block starting after the given position, handling nested braces.
/// Returns (open_brace_pos, close_brace_pos) or None if no block found.
fn find_block_after(code: &str, start: usize) -> Option<(usize, usize)> {
    let mut pos = start;
    while pos < code.len() && code.as_bytes()[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if pos >= code.len() || code.as_bytes()[pos] != b'{' {
        return None;
    }
    let block_start = pos;
    let mut depth = 1;
    pos += 1;
    while pos < code.len() && depth > 0 {
        match code.as_bytes()[pos] {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            b'"' => {
                pos += 1;
                while pos < code.len() && code.as_bytes()[pos] != b'"' {
                    if code.as_bytes()[pos] == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
            }
            b'\'' => {
                pos += 1;
                while pos < code.len() && code.as_bytes()[pos] != b'\'' {
                    if code.as_bytes()[pos] == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
            }
            b'`' => {
                pos += 1;
                while pos < code.len() && code.as_bytes()[pos] != b'`' {
                    if code.as_bytes()[pos] == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
            }
            b'/' if pos + 1 < code.len() && code.as_bytes()[pos + 1] == b'/' => {
                while pos < code.len() && code.as_bytes()[pos] != b'\n' {
                    pos += 1;
                }
            }
            b'/' if pos + 1 < code.len() && code.as_bytes()[pos + 1] == b'*' => {
                pos += 2;
                while pos + 1 < code.len()
                    && !(code.as_bytes()[pos] == b'*' && code.as_bytes()[pos + 1] == b'/')
                {
                    pos += 1;
                }
                pos += 1;
            }
            _ => {}
        }
        pos += 1;
    }
    if depth == 0 {
        Some((block_start, pos - 1))
    } else {
        None
    }
}

/// Replace compile-time constants defined in config.define.
/// Replaces all occurrences of each key with its corresponding value.
/// Values are JSON-parsed to determine if they should be string literals, numbers, or booleans.
pub(super) fn apply_define(
    code: &str,
    define: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = code.to_string();
    for (key, value) in define {
        let replacement = if value == "true"
            || value == "false"
            || value.parse::<f64>().is_ok()
            || value.starts_with('"')
            || value.starts_with('\'')
        {
            value.clone()
        } else {
            format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
        };
        result = result.replace(key, &replacement);
    }
    result
}

/// Expand import.meta.glob() calls into static module maps.
///
/// Supports two forms:
///   - `import.meta.glob('./pages/*.tsx')` → `{ './pages/Home.tsx': () => import('./pages/Home.tsx') }`
///   - `import.meta.glob('./pages/*.tsx', { eager: true })` → `{ './pages/Home.tsx': module0 }` with static imports
///
/// Also supports `{ query: '?raw', import: 'default' }` options for raw string imports.
pub(super) fn expand_import_meta_glob(
    code: &str,
    file_path: &str,
    config: &PledgeConfig,
) -> String {
    if !code.contains("import.meta.glob") {
        return code.to_string();
    }

    let file_dir = Path::new(file_path).parent().unwrap_or(Path::new("."));
    let root = &config.root;

    let mut result = code.to_string();
    let mut imports_prefix = String::new();

    while let Some(pos) = result.find("import.meta.glob(") {
        let args_start = pos + "import.meta.glob(".len();
        let mut depth = 1;
        let mut args_end = args_start;
        for (i, ch) in result[args_start..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        args_end = args_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }

        if depth != 0 {
            break;
        }

        let args_str = &result[args_start..args_end];

        let glob_pattern = match extract_glob_pattern(args_str) {
            Some(p) => p,
            None => {
                result.replace_range(pos..args_end + 1, "{}");
                continue;
            }
        };

        let eager = args_str.contains("eager:") && args_str.contains("true");
        let is_raw = args_str.contains("query:") && args_str.contains("raw");
        let import_filter = if args_str.contains("import:") {
            extract_import_filter(args_str)
        } else {
            "default"
        };

        let glob_base = if glob_pattern.starts_with('/') {
            root.join(glob_pattern.trim_start_matches('/'))
        } else {
            file_dir.join(&glob_pattern)
        };

        let matched_files = glob_files(&glob_base, root);

        if matched_files.is_empty() {
            result.replace_range(pos..args_end + 1, "{}");
            continue;
        }

        let mut map_entries = Vec::new();
        for (i, (rel_path, abs_path)) in matched_files.iter().enumerate() {
            if eager {
                let var_name = format!("__pledge_glob_{}", i);
                if is_raw {
                    // TODO(#fix): Make this transform fully async so file reads
                    // use tokio::task::spawn_blocking instead of blocking the
                    // worker thread. For now this runs on rayon workers (not
                    // the tokio runtime), but synchronous I/O still stalls the
                    // transform pipeline. A future refactor should collect all
                    // raw paths and batch-read them via spawn_blocking.
                    let content = std::fs::read_to_string(abs_path).unwrap_or_default();
                    imports_prefix.push_str(&format!(
                        "const {} = {};\n",
                        var_name,
                        serde_json::to_string(&content).unwrap_or_else(|_| "\"\"".to_string())
                    ));
                } else {
                    imports_prefix
                        .push_str(&format!("import * as {} from '{}';\n", var_name, rel_path));
                }
                let export_value = if import_filter == "default" {
                    format!("{}.default", var_name)
                } else if import_filter == "*" {
                    var_name.clone()
                } else {
                    format!("{}.{}", var_name, import_filter)
                };
                map_entries.push(format!(
                    "{}: {}",
                    serde_json::to_string(rel_path).unwrap_or_else(|_| "\"\"".to_string()),
                    export_value
                ));
            } else {
                if is_raw {
                    // Non-eager + raw: generate a lazy runtime import instead of
                    // reading the file synchronously at build time. This avoids
                    // blocking the transform worker thread on file I/O for files
                    // that are only needed on demand at runtime.
                    // The `?raw` suffix is a convention for raw string imports.
                    map_entries.push(format!(
                        "{}: () => import('{}?raw').then(m => m.default)",
                        serde_json::to_string(rel_path).unwrap_or_else(|_| "\"\"".to_string()),
                        rel_path
                    ));
                } else {
                    map_entries.push(format!(
                        "{}: () => import('{}')",
                        serde_json::to_string(rel_path).unwrap_or_else(|_| "\"\"".to_string()),
                        rel_path
                    ));
                }
            }
        }

        let map_str = format!("{{ {} }}", map_entries.join(", "));
        result.replace_range(pos..args_end + 1, &map_str);
    }

    if !imports_prefix.is_empty() {
        format!("{}\n{}", imports_prefix, result)
    } else {
        result
    }
}

/// Extract the glob pattern string from import.meta.glob arguments
fn extract_glob_pattern(args: &str) -> Option<String> {
    let trimmed = args.trim();
    for quote in ['"', '\''] {
        if trimmed.starts_with(quote)
            && let Some(end) = trimmed[1..].find(quote)
        {
            return Some(trimmed[1..1 + end].to_string());
        }
    }
    None
}

/// Extract the import filter from options (e.g., { import: 'default' })
fn extract_import_filter(args: &str) -> &str {
    if let Some(pos) = args.find("import:") {
        let rest = &args[pos + 7..];
        let trimmed = rest.trim();
        for quote in ['"', '\''] {
            if trimmed.starts_with(quote)
                && let Some(end) = trimmed[1..].find(quote)
            {
                let val = &trimmed[1..1 + end];
                return match val {
                    "default" => "default",
                    "*" => "*",
                    "named" => "named",
                    _ => "default",
                };
            }
        }
    }
    "default"
}

/// Glob-match files against a pattern with * and ** wildcards using globset
fn glob_files(pattern: &Path, root: &Path) -> Vec<(String, std::path::PathBuf)> {
    let pattern_str = crate::normalize_path(pattern);
    let mut results = Vec::new();

    let parts: Vec<&str> = pattern_str.split('/').collect();
    let mut base_dir = std::path::PathBuf::new();
    let mut wildcard_start = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.contains('*') || part.contains('?') || part.contains('{') {
            wildcard_start = i;
            break;
        }
        if !part.is_empty() {
            base_dir = base_dir.join(part);
        }
    }

    if !base_dir.is_dir() {
        return results;
    }

    let glob_pattern = parts[wildcard_start..].join("/");
    let glob = match Glob::new(&glob_pattern) {
        Ok(g) => g,
        Err(_) => return results,
    };
    let glob_matcher = glob.compile_matcher();

    glob_walk(&base_dir, &glob_matcher, root, &mut results);
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Generate a `pledge-env.d.ts` file declaring `import.meta.env` types.
///
/// This produces TypeScript ambient declarations so editors and `tsc` know
/// the shape of `import.meta.env` at build time. Each env var is typed based on
/// its value: numbers → `number`, booleans → `boolean`, everything else → `string`.
///
/// This complements [`EnvVars::generate_dts`] (which includes built-in vars and
/// is used during the build) by providing a standalone, config-driven entry
/// point that the transform pipeline can call when `config.env_dts` is enabled.
pub fn generate_env_dts(env_vars: &[(String, String)]) -> String {
    let mut content = String::from("/// <reference types=\"vite/client\" />\n\n");
    content.push_str("interface ImportMetaEnv {\n");
    for (key, value) in env_vars {
        let type_str = if value.parse::<i64>().is_ok() {
            "number"
        } else if value.parse::<bool>().is_ok() {
            "boolean"
        } else {
            "string"
        };
        content.push_str(&format!("  readonly {}: {}\n", key, type_str));
    }
    content.push_str("}\n\n");
    content.push_str("interface ImportMeta {\n");
    content.push_str("  readonly env: ImportMetaEnv;\n");
    content.push_str("}\n");
    content
}

/// Recursively walk a directory and collect files matching a globset matcher
fn glob_walk(
    current_dir: &Path,
    matcher: &globset::GlobMatcher,
    root: &Path,
    results: &mut Vec<(String, std::path::PathBuf)>,
) {
    if let Ok(entries) = std::fs::read_dir(current_dir) {
        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "node_modules" || name == "target" || name.starts_with('.') {
                    continue;
                }
                glob_walk(&path, matcher, root, results);
            } else if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if matcher.is_match(&name)
                    && let Ok(rel) = path.strip_prefix(root)
                {
                    let rel_str = crate::normalize_path(rel);
                    results.push((rel_str, path));
                }
            }
        }
    }
}
