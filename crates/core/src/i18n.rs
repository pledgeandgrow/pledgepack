// i18n-aware bundling (#106)
//
// Splits bundles by locale. Only loads the current locale's strings.
// Handles `import messages from './messages.${locale}.json' pattern.
//
// Also provides compile-time i18n key extraction (#13): parses .tsx/.ts/.jsx
// files with Oxc AST to find t('key') calls and builds a translation catalog.

use crate::config::I18nConfig;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tracing::info;

/// Resolve locale-specific message file path
pub fn resolve_locale_file(pattern: &str, locale: &str, root: &Path) -> PathBuf {
    let path = pattern.replace("${locale}", locale);
    if path.starts_with("./") {
        root.join(path.strip_prefix("./").unwrap_or(&path))
    } else {
        PathBuf::from(path)
    }
}

/// Transform import statements that use ${locale} pattern
/// Replaces `import messages from './messages.${locale}.json'`
/// with a dynamic import based on the detected locale at runtime
pub fn transform_i18n_imports(code: &str, config: &I18nConfig) -> String {
    if !config.enabled || config.locales.is_empty() {
        return code.to_string();
    }

    let mut result = code.to_string();
    let pattern = &config.message_pattern;

    // Replace static imports of ${locale} pattern with runtime locale detection
    let _import_pattern = pattern.replace("${locale}", "\\$\\{locale\\}");

    // Transform: import messages from './messages.${locale}.json'
    // Into: const messages = await import(`./messages.${locale}.json`)
    // with a runtime locale detection shim
    if result.contains(pattern) {
        // Generate locale detection shim + dynamic import
        let locales_json = serde_json::to_string(&config.locales).unwrap_or_default();
        let default_locale = &config.default_locale;

        let shim = format!(
            r#"// i18n locale detection (#106)
const __pledge_locales = {};
const __pledge_defaultLocale = "{}";
const __pledge_locale = (typeof navigator !== 'undefined' && navigator.language)
  ? navigator.language.split('-')[0]
  : __pledge_defaultLocale;
const __pledge_currentLocale = __pledge_locales.includes(__pledge_locale) ? __pledge_locale : __pledge_defaultLocale;
"#,
            locales_json, default_locale,
        );

        // Replace the pattern in import statements
        let resolved_pattern = pattern.replace("${locale}", "${__pledge_currentLocale}");
        result = result.replace(pattern, &resolved_pattern);

        // Prepend the shim
        result = format!("{}\n{}", shim, result);
    }

    info!(
        "i18n transform: {} locales configured",
        config.locales.len()
    );
    result
}

/// Generate locale-specific entry chunks
pub fn generate_locale_entries(config: &I18nConfig) -> Vec<String> {
    if !config.enabled {
        return Vec::new();
    }
    config.locales.clone()
}

/// Check if a file path matches the i18n message pattern
pub fn is_locale_message_file(path: &str, config: &I18nConfig) -> bool {
    if !config.enabled {
        return false;
    }
    // Check if path matches the pattern (with any locale substituted)
    let pattern = &config.message_pattern;
    let pattern_base = pattern.replace("${locale}", "");

    // Normalize path for comparison
    let path_normalized = crate::normalize_path_str(path);

    for locale in &config.locales {
        let expected = pattern.replace("${locale}", locale);
        let expected_normalized = crate::normalize_path_str(&expected);
        if path_normalized.ends_with(&expected_normalized) || path_normalized == expected_normalized
        {
            return true;
        }
    }

    // Also check if the path contains the pattern base
    if path_normalized.contains(&pattern_base.replace("./", "")) {
        return true;
    }

    false
}

// ─── Compile-time i18n key extraction (#13) ──────────────────────────────

/// Extracted translation key with metadata
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractedKey {
    /// The translation key string (e.g., "greeting.hello")
    pub key: String,
    /// Source file where the key was found
    pub file: String,
    /// Line number (1-indexed)
    pub line: u32,
    /// Column number (1-indexed)
    pub column: u32,
}

/// Result of i18n extraction from a single source file
#[derive(Debug, Clone, Default)]
pub struct ExtractionResult {
    /// All translation keys found in this file
    pub keys: Vec<ExtractedKey>,
}

/// Catalog of all extracted translation keys across the project
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TranslationCatalog {
    /// Map of key → list of source locations
    pub keys: BTreeMap<String, Vec<ExtractedKey>>,
}

impl TranslationCatalog {
    /// Merge another catalog into this one
    pub fn merge(&mut self, other: TranslationCatalog) {
        for (key, locations) in other.keys {
            self.keys.entry(key).or_default().extend(locations);
        }
    }

    /// Serialize the catalog to a JSON string
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Get all unique keys
    pub fn all_keys(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    /// Number of unique keys
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the catalog is empty
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Extract translation keys from source code using Oxc AST parsing.
///
/// Detects these patterns:
/// - `t('key')` / `t("key")` — direct call with string literal
/// - `t(`key`)` — template literal without interpolation
/// - `i18n.t('key')` — namespaced call
/// - `useTranslation().t('key')` — via hook (react-i18next)
/// - `Trans i18nKey="key"` — JSX prop (react-i18next)
///
/// Falls back to string-based detection if AST parsing fails.
pub fn extract_i18n_keys(source: &str, file_path: &str) -> ExtractionResult {
    // Try AST-based extraction first
    if let Some(result) = extract_i18n_keys_ast(source, file_path) {
        return result;
    }

    // Fallback: string-based detection
    extract_i18n_keys_string(source, file_path)
}

/// AST-based extraction using Oxc
fn extract_i18n_keys_ast(source: &str, file_path: &str) -> Option<ExtractionResult> {
    use oxc::allocator::Allocator;
    use oxc::ast_visit::Visit;
    use oxc::parser::{Parser, ParserReturn};
    use oxc::span::SourceType;

    let allocator = Allocator::default();
    let path = Path::new(file_path);
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx());

    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, source, source_type).parse();

    if panicked {
        return None;
    }

    struct I18nVisitor<'s> {
        source: &'s str,
        file: &'s str,
        keys: Vec<ExtractedKey>,
    }

    impl<'a, 's> Visit<'a> for I18nVisitor<'s> {
        fn visit_call_expression(&mut self, expr: &oxc::ast::ast::CallExpression<'a>) {
            // Check if the callee is `t` or `*.t` (e.g., `i18n.t`, `t`)
            let is_t_call = match &expr.callee {
                oxc::ast::ast::Expression::Identifier(ident) => ident.name == "t",
                oxc::ast::ast::Expression::StaticMemberExpression(member) => {
                    member.property.name == "t"
                }
                oxc::ast::ast::Expression::ComputedMemberExpression(member) => {
                    if let oxc::ast::ast::Expression::StringLiteral(lit) = &member.expression {
                        lit.value == "t"
                    } else {
                        false
                    }
                }
                _ => false,
            };

            if is_t_call
                && let Some(arg) = expr.arguments.first()
                && let Some(key) = extract_string_from_argument(arg)
            {
                let span = oxc::span::GetSpan::span(arg);
                let (line, column) = span_to_line_col(self.source, span.start);
                self.keys.push(ExtractedKey {
                    key,
                    file: self.file.to_string(),
                    line,
                    column,
                });
            }

            // Continue visiting children
            self.visit_expression(&expr.callee);
            self.visit_arguments(&expr.arguments);
        }

        fn visit_jsx_opening_element(&mut self, element: &oxc::ast::ast::JSXOpeningElement<'a>) {
            // Check for <Trans i18nKey="key" /> pattern
            for attr in &element.attributes {
                if let oxc::ast::ast::JSXAttributeItem::Attribute(attr) = attr {
                    let is_i18n_key = match &attr.name {
                        oxc::ast::ast::JSXAttributeName::Identifier(ident) => {
                            ident.name == "i18nKey"
                        }
                        _ => false,
                    };
                    if is_i18n_key
                        && let Some(oxc::ast::ast::JSXAttributeValue::StringLiteral(lit)) =
                            &attr.value
                    {
                        let (line, column) = span_to_line_col(self.source, attr.span.start);
                        self.keys.push(ExtractedKey {
                            key: lit.value.to_string(),
                            file: self.file.to_string(),
                            line,
                            column,
                        });
                    }
                }
            }

            // Continue visiting children
            self.visit_jsx_element_name(&element.name);
            self.visit_jsx_attribute_items(&element.attributes);
        }
    }

    /// Extract a string from an argument: string literal or template literal (no interpolation)
    fn extract_string_from_argument<'a>(arg: &oxc::ast::ast::Argument<'a>) -> Option<String> {
        match arg {
            oxc::ast::ast::Argument::StringLiteral(lit) => Some(lit.value.to_string()),
            oxc::ast::ast::Argument::TemplateLiteral(lit) => {
                // Only extract if there are no expressions (no interpolation)
                if lit.expressions.is_empty() && lit.quasis.len() == 1 {
                    lit.quasis[0].value.cooked.as_deref().map(|s| s.to_string())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Convert byte offset to 1-indexed line and column
    fn span_to_line_col(source: &str, offset: u32) -> (u32, u32) {
        let bytes = source.as_bytes();
        let offset = (offset as usize).min(bytes.len());
        let mut line = 1u32;
        let mut col = 1u32;
        for &b in &bytes[..offset] {
            if b == b'\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    let mut visitor = I18nVisitor {
        source,
        file: file_path,
        keys: Vec::new(),
    };
    visitor.visit_program(&program);

    Some(ExtractionResult { keys: visitor.keys })
}

/// Fallback: string-based extraction using regex-like patterns
fn extract_i18n_keys_string(source: &str, file_path: &str) -> ExtractionResult {
    let mut keys = Vec::new();
    let mut search_pos = 0;

    while let Some(pos) = source[search_pos..].find("t(") {
        let abs_pos = search_pos + pos;
        let after = &source[abs_pos + 2..];

        // Try to extract a string literal argument
        if let Some(quote_pos) = after.find(['"', '\'']) {
            let quote_char = after.as_bytes()[quote_pos] as char;
            let arg_start = quote_pos + 1;
            if let Some(end) = after[arg_start..].find(quote_char) {
                let key = &after[arg_start..arg_start + end];
                // Validate key: no whitespace-only, not empty, looks like a translation key
                if !key.is_empty() && !key.contains('\n') && key.chars().all(|c| !c.is_control()) {
                    let (line, column) = byte_offset_to_line_col(source, abs_pos);
                    keys.push(ExtractedKey {
                        key: key.to_string(),
                        file: file_path.to_string(),
                        line,
                        column,
                    });
                }
            }
        }

        // Also check for i18nKey="..." in JSX
        search_pos = abs_pos + 2;
    }

    // Also extract i18nKey="key" from JSX
    let mut jsx_pos = 0;
    while let Some(pos) = source[jsx_pos..].find("i18nKey=") {
        let abs_pos = jsx_pos + pos;
        let after = &source[abs_pos + 8..];

        // Skip optional whitespace
        let after = after.trim_start();
        if after.starts_with('"') || after.starts_with('\'') {
            let quote_char = after.as_bytes()[0] as char;
            if let Some(end) = after[1..].find(quote_char) {
                let key = &after[1..1 + end];
                if !key.is_empty() {
                    let (line, column) = byte_offset_to_line_col(source, abs_pos);
                    keys.push(ExtractedKey {
                        key: key.to_string(),
                        file: file_path.to_string(),
                        line,
                        column,
                    });
                }
            }
        }

        jsx_pos = abs_pos + 8;
    }

    ExtractionResult { keys }
}

/// Convert byte offset to 1-indexed line and column
fn byte_offset_to_line_col(source: &str, offset: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut col = 1u32;
    for &b in source.as_bytes().iter().take(offset) {
        if b == b'\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// Write the translation catalog to a JSON file
pub fn write_catalog(catalog: &TranslationCatalog, output_path: &Path) -> std::io::Result<()> {
    std::fs::write(output_path, catalog.to_json())
}
