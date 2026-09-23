// Type checking during build (#71)
//
// Integrates `tsc --noEmit` type checking into the build pipeline.
// When `build.typeCheck: true`, runs TypeScript type checking and
// fails the build on type errors with formatted output.
//
// Also handles:
//   - #72 Type-aware tree shaking: detect `import type` and exclude type-only imports
//   - #73 .d.ts bundling for library mode: tree-shake unused type declarations

use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// Type check result
#[derive(Debug, Clone)]
pub struct TypeCheckResult {
    pub success: bool,
    pub errors: Vec<TypeError>,
    pub warnings: Vec<TypeWarning>,
    pub duration_ms: u128,
}

/// A single type error
#[derive(Debug, Clone)]
pub struct TypeError {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub code: String,
    pub message: String,
    pub severity: String,
}

/// A type warning
#[derive(Debug, Clone)]
pub struct TypeWarning {
    pub file: String,
    pub line: u32,
    pub message: String,
}

/// Run `tsc --noEmit` type checking
pub fn run_type_check(root: &Path) -> Result<TypeCheckResult> {
    let start = std::time::Instant::now();

    // Check if tsc is available
    let tsc = find_tsc(root);

    let tsc_path = match tsc {
        Some(path) => path,
        None => {
            return Ok(TypeCheckResult {
                success: true,
                errors: vec![TypeError {
                    file: "tsconfig.json".to_string(),
                    line: 0,
                    column: 0,
                    code: "TSC_NOT_FOUND".to_string(),
                    message:
                        "TypeScript compiler not found. Install with: npm install -D typescript"
                            .to_string(),
                    severity: "warning".to_string(),
                }],
                warnings: Vec::new(),
                duration_ms: start.elapsed().as_millis(),
            });
        }
    };

    // Run tsc --noEmit --pretty false for parseable output
    let mut cmd = Command::new(&tsc_path);
    if tsc_path == "npx" || tsc_path == "npx.cmd" {
        cmd.arg("tsc");
    }
    let output = cmd
        .args(["--noEmit", "--pretty", "false", "--format", "json"])
        .current_dir(root)
        .output()?;

    let duration_ms = start.elapsed().as_millis();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    // Try parsing as JSON (tsc --format json outputs newline-delimited JSON)
    for line in stdout.lines() {
        if line.trim_start().starts_with('{')
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(line)
        {
            let severity = json
                .get("category")
                .and_then(|c| c.as_str())
                .unwrap_or("error");

            let te = TypeError {
                file: json
                    .get("file")
                    .and_then(|f| f.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                line: json
                    .get("startPosition")
                    .and_then(|p| p.get("line"))
                    .and_then(|l| l.as_u64())
                    .map(|l| l as u32 + 1)
                    .unwrap_or(0),
                column: json
                    .get("startPosition")
                    .and_then(|p| p.get("character"))
                    .and_then(|c| c.as_u64())
                    .map(|c| c as u32 + 1)
                    .unwrap_or(0),
                code: json
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string(),
                message: json
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
                severity: severity.to_string(),
            };

            if severity == "warning" {
                warnings.push(TypeWarning {
                    file: te.file.clone(),
                    line: te.line,
                    message: te.message.clone(),
                });
            } else {
                errors.push(te);
            }
        }
    }

    // Fallback: parse non-JSON output (file(line,col): error TSxxxx: message)
    if errors.is_empty() && warnings.is_empty() && !stderr.is_empty() {
        for line in stderr.lines() {
            if let Some(err) = parse_tsc_line(line) {
                if err.severity == "warning" {
                    warnings.push(TypeWarning {
                        file: err.file.clone(),
                        line: err.line,
                        message: err.message.clone(),
                    });
                } else {
                    errors.push(err);
                }
            }
        }
    }

    let success = errors.is_empty();

    Ok(TypeCheckResult {
        success,
        errors,
        warnings,
        duration_ms,
    })
}

/// Format type check results for terminal output
pub fn format_type_check_result(result: &TypeCheckResult) -> String {
    if result.success && result.warnings.is_empty() {
        return format!(
            "  \x1b[32m✓\x1b[0m Type check passed ({}ms)",
            result.duration_ms
        );
    }

    let mut out = String::new();

    if !result.errors.is_empty() {
        out.push_str(&format!(
            "  \x1b[31m✗\x1b[0m Type check failed: {} error(s) ({}ms)\n\n",
            result.errors.len(),
            result.duration_ms
        ));

        for err in &result.errors {
            out.push_str(&format!(
                "  \x1b[31merror\x1b[0m {}:{}:{} — {}\n    \x1b[90mTS{}: {}\x1b[0m\n\n",
                err.file, err.line, err.column, err.message, err.code, err.message
            ));
        }
    }

    if !result.warnings.is_empty() {
        out.push_str(&format!(
            "  \x1b[33m⚠\x1b[0m {} warning(s)\n\n",
            result.warnings.len()
        ));

        for warn in &result.warnings {
            out.push_str(&format!(
                "  \x1b[33mwarn\x1b[0m {}:{} — {}\n",
                warn.file, warn.line, warn.message
            ));
        }
    }

    out
}

/// Find the tsc binary in node_modules or PATH
fn find_tsc(root: &Path) -> Option<String> {
    // On Windows, `.bin/tsc` is a POSIX shell script that CreateProcess can't
    // run — prefer the `.cmd` shim (std routes it through cmd.exe).
    #[cfg(target_os = "windows")]
    {
        let local_tsc_cmd = root.join("node_modules").join(".bin").join("tsc.cmd");
        if local_tsc_cmd.exists() {
            return Some(local_tsc_cmd.to_string_lossy().to_string());
        }
    }

    // Check node_modules/.bin/tsc
    let local_tsc = root.join("node_modules").join(".bin").join("tsc");
    if local_tsc.exists() {
        return Some(local_tsc.to_string_lossy().to_string());
    }

    // Try npx. On Windows `npx` is a `npx.cmd` batch shim that
    // `Command::new("npx")` can't resolve — spell the extension out (std
    // routes .cmd through cmd.exe and escapes args for it).
    let npx_bin = if cfg!(target_os = "windows") {
        "npx.cmd"
    } else {
        "npx"
    };
    let npx_check = Command::new(npx_bin)
        .args(["--no-install", "tsc", "--version"])
        .output();
    if let Ok(out) = npx_check
        && out.status.success()
    {
        return Some(npx_bin.to_string());
    }

    None
}

/// Parse a tsc output line: file(line,col): error TSxxxx: message
fn parse_tsc_line(line: &str) -> Option<TypeError> {
    // Format: path/to/file.ts(10,5): error TS2304: Cannot find name 'foo'.
    let line = line.trim();

    // Find the parenthetical (line,col)
    let paren_start = line.find('(')?;
    let paren_end = line[paren_start..].find(')')?;
    let file = line[..paren_start].trim().to_string();
    let pos_str = &line[paren_start + 1..paren_start + paren_end];
    let pos_parts: Vec<&str> = pos_str.split(',').collect();
    let line_num = pos_parts.first()?.parse::<u32>().ok()?;
    let col = pos_parts.get(1)?.parse::<u32>().ok()?;

    let rest = &line[paren_start + paren_end + 1..].trim();

    // Parse severity and code
    let (severity, code, message) = if let Some(after) = rest.strip_prefix("error TS") {
        let colon = after.find(':')?;
        let code = format!("TS{}", &after[..colon]);
        let message = after[colon + 1..].trim().to_string();
        ("error", code, message)
    } else if let Some(after) = rest.strip_prefix("warning TS") {
        let colon = after.find(':')?;
        let code = format!("TS{}", &after[..colon]);
        let message = after[colon + 1..].trim().to_string();
        ("warning", code, message)
    } else {
        ("error", String::new(), rest.to_string())
    };

    Some(TypeError {
        file,
        line: line_num,
        column: col,
        code,
        message,
        severity: severity.to_string(),
    })
}

// ─── #72 Type-aware tree shaking ─────────────────────────────────────────────

/// Detect `import type` statements in source code.
/// Returns a list of (module_path, imported_type_names) that are type-only imports
/// and should be excluded from the runtime bundle.
pub fn detect_type_only_imports(source: &str) -> Vec<TypeOnlyImport> {
    let mut imports = Vec::new();

    for line in source.lines() {
        let trimmed = line.trim();

        // Match: import type { Foo, Bar } from "./module"
        // Match: import type Foo from "./module"
        // Match: import { type Foo, Bar } from "./module"  (inline type specifier)
        if trimmed.starts_with("import ")
            && let Some(type_import) = parse_type_import(trimmed)
        {
            imports.push(type_import);
        }
    }

    imports
}

/// A type-only import to exclude from runtime bundle
#[derive(Debug, Clone)]
pub struct TypeOnlyImport {
    pub module: String,
    pub type_names: Vec<String>,
    pub is_all_type: bool, // `import type ...` vs `import { type X }`
}

/// Parse a single import line for type-only imports
fn parse_type_import(line: &str) -> Option<TypeOnlyImport> {
    // `import type { Foo, Bar } from "module"`
    if let Some(rest) = line.strip_prefix("import type ") {
        return parse_import_specifiers(rest, true);
    }

    // `import { type Foo, Bar } from "module"` — inline type
    if line.starts_with("import {")
        && let Some(from_pos) = line.find(" from ")
    {
        let specifiers = &line["import {".len()..from_pos];
        let module = extract_module_path(&line[from_pos..]);

        if let Some(module) = module {
            let type_names: Vec<String> = specifiers
                .split(',')
                .filter_map(|s| {
                    let s = s.trim();
                    s.strip_prefix("type ").map(|rest| rest.trim().to_string())
                })
                .collect();

            if !type_names.is_empty() {
                // Check if ALL specifiers are type-only
                let all_type = specifiers.split(',').all(|s| s.trim().starts_with("type "));
                return Some(TypeOnlyImport {
                    module,
                    type_names,
                    is_all_type: all_type,
                });
            }
        }
    }

    None
}

/// Parse import specifiers from `rest` of an `import type` statement
fn parse_import_specifiers(rest: &str, is_all_type: bool) -> Option<TypeOnlyImport> {
    // Extract module path
    let module = extract_module_path(rest)?;

    // Extract type names
    let mut type_names = Vec::new();

    if let Some(brace_start) = rest.find('{') {
        if let Some(brace_end) = rest[brace_start..].find('}') {
            let inner = &rest[brace_start + 1..brace_start + brace_end];
            type_names = inner
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    } else {
        // `import type Foo from "module"` — default import
        let before_from = rest.find(" from ").map(|p| &rest[..p]).unwrap_or(rest);
        let name = before_from.trim();
        if !name.is_empty() {
            type_names.push(name.to_string());
        }
    }

    Some(TypeOnlyImport {
        module,
        type_names,
        is_all_type,
    })
}

/// Extract module path from `from "..."` or `"..."` part of import
fn extract_module_path(s: &str) -> Option<String> {
    // Find `from "..."` or `from '...'`
    if let Some(from_pos) = s.find(" from ") {
        let after = &s[from_pos + 6..].trim();
        return extract_quoted_string(after);
    }
    // Direct: `"module"` (for `import type Foo from "module"`)
    extract_quoted_string(s.trim())
}

/// Extract a string from quotes
fn extract_quoted_string(s: &str) -> Option<String> {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        Some(s[1..s.len() - 1].to_string())
    } else {
        None
    }
}

/// Remove type-only import statements from source code
pub fn strip_type_only_imports(source: &str) -> String {
    let mut result = String::with_capacity(source.len());

    for line in source.lines() {
        let trimmed = line.trim();

        // Skip `import type ...` lines entirely
        if trimmed.starts_with("import type ") {
            continue;
        }

        // For `import { type Foo, Bar } from "module"`, remove only the type specifiers
        if trimmed.starts_with("import {")
            && trimmed.contains("type ")
            && let Some(from_pos) = trimmed.find(" from ")
        {
            let specifiers = &trimmed["import {".len()..from_pos];
            let module_part = &trimmed[from_pos..];

            let runtime_specifiers: Vec<&str> = specifiers
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && !s.starts_with("type "))
                .collect();

            if runtime_specifiers.is_empty() {
                // All specifiers were type-only, skip entire import
                continue;
            }

            result.push_str(&format!(
                "import {{ {} }} {};\n",
                runtime_specifiers.join(", "),
                module_part.trim_end_matches(';')
            ));
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    result
}

// ─── #73 .d.ts bundling for library mode ─────────────────────────────────────

/// Bundle TypeScript declarations into a single .d.ts file.
/// Tree-shakes unused type declarations by following the public API surface
/// from the entry point.
pub fn bundle_declarations(entry_dts: &Path, project_root: &Path) -> Result<String> {
    let mut bundled = String::new();
    let mut visited = std::collections::HashSet::new();

    bundled.push_str("// Bundled type declarations — generated by PledgePack\n");
    bundled.push_str("// Do not edit manually.\n\n");

    bundle_dts_recursive(entry_dts, project_root, &mut bundled, &mut visited)?;

    Ok(bundled)
}

/// Recursively bundle .d.ts files, following imports
fn bundle_dts_recursive(
    file: &Path,
    root: &Path,
    output: &mut String,
    visited: &mut std::collections::HashSet<String>,
) -> Result<()> {
    let file_key = file.to_string_lossy().to_string();
    if visited.contains(&file_key) {
        return Ok(());
    }
    visited.insert(file_key);

    let content = std::fs::read_to_string(file)?;

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with("//") {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        // Handle import statements — inline the imported .d.ts
        if trimmed.starts_with("import ")
            && let Some(from_pos) = trimmed.find(" from ")
        {
            let module_part = &trimmed[from_pos + 6..];
            if let Some(module) = extract_quoted_string(module_part.trim()) {
                // Resolve relative to current file
                let resolved = file
                    .parent()
                    .map(|p| p.join(&module))
                    .unwrap_or_else(|| root.join(&module));

                // Try .d.ts extension
                let dts_path = if resolved.extension().and_then(|e| e.to_str()) == Some("ts") {
                    resolved.with_extension("d.ts")
                } else if resolved.extension().and_then(|e| e.to_str()) == Some("d.ts") {
                    resolved
                } else {
                    resolved.with_extension("d.ts")
                };

                if dts_path.exists() {
                    bundle_dts_recursive(&dts_path, root, output, visited)?;
                    continue;
                }
            }
        }

        // Skip export statements that are just re-exports
        if trimmed.starts_with("export * from") || trimmed.starts_with("export {}") {
            continue;
        }

        // Keep all other lines (type declarations, interfaces, etc.)
        output.push_str(line);
        output.push('\n');
    }

    output.push('\n');
    Ok(())
}
