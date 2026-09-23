//! Clean, human-readable source diagnostics.
//!
//! Parser errors used to leak as raw Rust `Debug` output
//! (`OxcDiagnostic { inner: OxcDiagnosticInner { ... } }`). This module turns
//! a message plus a byte offset into:
//!
//! ```text
//! error: Unexpected token
//!   --> src/index.tsx:57:11
//!    |
//! 57 |   return <div>;;
//!    |                ^
//! ```

use std::fmt;
use std::path::Path;

/// A single rendered source error (message + location + code frame).
#[derive(Debug, Clone)]
pub struct SourceDiagnostic {
    /// The bare message, e.g. `Unexpected token`.
    pub message: String,
    /// The file as shown to the user (project-relative when possible).
    pub file: String,
    /// 1-based line, or 0 when unknown.
    pub line: usize,
    /// 1-based column, or 0 when unknown.
    pub column: usize,
    /// The rendered `-->` header plus code frame (may be empty).
    pub frame: String,
    /// Optional help text.
    pub help: Option<String>,
}

impl fmt::Display for SourceDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error: {}", self.message)?;
        if !self.frame.is_empty() {
            write!(f, "\n{}", self.frame)?;
        }
        if let Some(help) = &self.help {
            write!(f, "\n  help: {}", help)?;
        }
        Ok(())
    }
}

impl std::error::Error for SourceDiagnostic {}

/// A bundle of one or more [`SourceDiagnostic`]s for a single file.
#[derive(Debug, Clone)]
pub struct SourceDiagnostics(pub Vec<SourceDiagnostic>);

impl fmt::Display for SourceDiagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, "\n\n")?;
            }
            write!(f, "{}", d)?;
        }
        Ok(())
    }
}

impl std::error::Error for SourceDiagnostics {}

/// Convert a byte offset to a 1-based (line, column) pair. Columns count
/// characters, not bytes.
pub fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    // Snap to a char boundary.
    let mut off = offset;
    while off > 0 && !source.is_char_boundary(off) {
        off -= 1;
    }
    let before = &source[..off];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// Extract `(line, column)` from the first `--> file:LINE:COL` header in a
/// rendered diagnostic (used by the dev-server error overlay).
pub fn parse_location(rendered: &str) -> Option<(usize, usize)> {
    let header = rendered
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("--> "))?;
    let mut parts = header.rsplitn(3, ':');
    let col = parts.next()?.trim().parse().ok()?;
    let line = parts.next()?.trim().parse().ok()?;
    Some((line, col))
}

/// Project-relative, forward-slash display of `file`.
pub fn display_file(file: &str, root: &Path) -> String {
    let p = Path::new(file);
    let rel = p.strip_prefix(root).unwrap_or(p);
    crate::normalize_path(rel)
}

/// Render a code frame for `line`/`column` (1-based) in `source`.
///
/// `span_len` is the number of characters to underline (at least one caret).
pub fn code_frame(file: &str, source: &str, line: usize, column: usize, span_len: usize) -> String {
    let header = format!("  --> {}:{}:{}", file, line, column);
    let Some(text) = source.lines().nth(line.saturating_sub(1)) else {
        return header;
    };
    let width = line.to_string().len();
    let text = text.trim_end_matches('\r');
    let expanded: String = text.replace('\t', "    ");
    // Column is in chars of the original line; tabs expand to 4 columns.
    let tabs_before = text
        .chars()
        .take(column.saturating_sub(1))
        .filter(|c| *c == '\t')
        .count();
    let pad = column.saturating_sub(1) + tabs_before * 3;
    let line_chars = expanded.chars().count();
    let carets = span_len.max(1).min(line_chars.saturating_sub(pad).max(1));
    format!(
        "{header}\n{gutter} |\n{line:>width$} | {expanded}\n{gutter} | {pad}{carets}",
        gutter = " ".repeat(width),
        line = line,
        width = width,
        pad = " ".repeat(pad),
        carets = "^".repeat(carets),
    )
}

/// Build a diagnostic from a byte offset/length.
pub fn diagnostic_at(
    file: &str,
    source: &str,
    offset: usize,
    len: usize,
    message: impl Into<String>,
    help: Option<String>,
) -> SourceDiagnostic {
    let (line, column) = line_col(source, offset);
    let end = (offset + len).min(source.len());
    let start = offset.min(source.len());
    let span_chars = source
        .get(start..end)
        .map(|s| s.lines().next().unwrap_or("").chars().count())
        .unwrap_or(1);
    SourceDiagnostic {
        message: message.into(),
        file: file.to_string(),
        line,
        column,
        frame: code_frame(file, source, line, column, span_chars),
        help,
    }
}

/// Render oxc parser/transform diagnostics for `file_path`.
pub fn from_oxc<'a>(
    file_path: &str,
    source: &str,
    root: &Path,
    diagnostics: impl IntoIterator<Item = &'a oxc::diagnostics::OxcDiagnostic>,
) -> SourceDiagnostics {
    let file = display_file(file_path, root);
    let out = diagnostics
        .into_iter()
        .map(|d| {
            let help = d.help.as_ref().map(|h| h.to_string());
            match d.labels.as_slice().first() {
                Some(label) => diagnostic_at(
                    &file,
                    source,
                    label.offset() as usize,
                    label.len() as usize,
                    d.message.to_string(),
                    help,
                ),
                None => SourceDiagnostic {
                    message: d.message.to_string(),
                    file: file.clone(),
                    line: 0,
                    column: 0,
                    frame: format!("  --> {}", file),
                    help,
                },
            }
        })
        .collect();
    SourceDiagnostics(out)
}

/// Render a CSS parse error. `line0` is the 0-based line reported by
/// lightningcss and `column` its 1-based column.
pub fn css_error(
    file_path: &str,
    source: &str,
    root: &Path,
    message: &str,
    loc: Option<(u32, u32)>,
) -> SourceDiagnostic {
    let file = display_file(file_path, root);
    match loc {
        Some((line0, column)) => {
            let line = line0 as usize + 1;
            SourceDiagnostic {
                message: message.to_string(),
                file: file.clone(),
                line,
                column: column as usize,
                frame: code_frame(&file, source, line, column as usize, 1),
                help: None,
            }
        }
        None => SourceDiagnostic {
            message: message.to_string(),
            file: file.clone(),
            line: 0,
            column: 0,
            frame: format!("  --> {}", file),
            help: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_counts_lines_and_chars() {
        let src = "aaa\nbéb ccc\nddd";
        assert_eq!(line_col(src, 0), (1, 1));
        // 'c' after "bé b " : bytes: a a a \n b é(2) b ' ' c
        let off = src.find('c').unwrap();
        assert_eq!(line_col(src, off), (2, 5));
        assert_eq!(line_col(src, src.len()), (3, 4));
    }

    #[test]
    fn location_is_recoverable_from_the_rendered_text() {
        let text = "error: Unexpected token
  --> C:/proj/src/a.tsx:57:11
   |
";
        assert_eq!(parse_location(text), Some((57, 11)));
        assert_eq!(parse_location("no header here"), None);
    }

    #[test]
    fn frame_has_caret_under_offset() {
        let src = "const a = 1;\nconst b = ;\n";
        let off = src.find(';').unwrap();
        let off = src[off + 1..].find(';').unwrap() + off + 1;
        let d = diagnostic_at("src/a.ts", src, off, 1, "Unexpected token", None);
        assert_eq!((d.line, d.column), (2, 11));
        let text = d.to_string();
        assert!(text.starts_with("error: Unexpected token\n  --> src/a.ts:2:11\n"));
        assert!(text.contains("2 | const b = ;"));
        let last = text.lines().last().unwrap();
        assert_eq!(last, format!("  | {}^", " ".repeat(10)));
    }

    #[test]
    fn oxc_errors_render_cleanly_without_debug_noise() {
        use oxc::allocator::Allocator;
        use oxc::parser::Parser;
        use oxc::span::SourceType;
        let src = "export const x = 1;\nconst y = ;\n";
        let alloc = Allocator::default();
        let ret = Parser::new(&alloc, src, SourceType::tsx()).parse();
        let rendered =
            from_oxc("/proj/src/x.tsx", src, Path::new("/proj"), &ret.diagnostics).to_string();
        assert!(rendered.starts_with("error: "), "{rendered}");
        assert!(rendered.contains("--> src/x.tsx:2:"), "{rendered}");
        assert!(rendered.contains("const y = ;"), "{rendered}");
        assert!(!rendered.contains("OxcDiagnostic"), "{rendered}");
        assert!(!rendered.contains("LabeledSpan"), "{rendered}");
    }
}
