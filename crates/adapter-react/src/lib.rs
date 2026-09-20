// React adapter: JSX transform + Fast Refresh injection
//
// Handles:
//   - JSX → React.createElement (or automatic jsx-runtime)
//   - TypeScript type stripping
//   - React Fast Refresh boundary detection
//   - HMR accept code injection

use anyhow::Result;
use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions};
use oxc::parser::{Parser, ParserReturn};
use oxc::span::SourceType;
use oxc::transformer::{JsxRuntime, TransformOptions, Transformer};
use pledgepack_core::module::ModuleKind;
use std::path::Path;

pub struct ReactAdapter {
    /// Use automatic JSX runtime (React 17+)
    automatic_runtime: bool,
}

impl Default for ReactAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ReactAdapter {
    pub fn new() -> Self {
        Self {
            automatic_runtime: true,
        }
    }

    /// Transform JSX/TSX for React using Oxc
    pub fn transform(
        &self,
        source: &str,
        kind: ModuleKind,
        file_path: &str,
        is_production: bool,
    ) -> Result<ReactTransformResult> {
        let allocator = Allocator::default();
        let path = Path::new(file_path);

        let source_type = SourceType::from_path(path).unwrap_or_else(|_| match kind {
            ModuleKind::Tsx | ModuleKind::Psx => SourceType::tsx(),
            ModuleKind::TypeScript => SourceType::ts(),
            ModuleKind::Jsx => SourceType::jsx(),
            _ => SourceType::mjs(),
        });

        let ParserReturn {
            mut program,
            diagnostics: parser_errors,
            panicked,
            ..
        } = Parser::new(&allocator, source, source_type).parse();

        if panicked || !parser_errors.is_empty() {
            anyhow::bail!(
                "Failed to parse {}: {}",
                file_path,
                parser_errors
                    .first()
                    .map(|e| e.to_string())
                    .unwrap_or("unknown".into())
            );
        }

        let mut options = TransformOptions::default();
        options.typescript.only_remove_type_imports = false;

        if self.automatic_runtime {
            options.jsx.runtime = JsxRuntime::Automatic;
            options.jsx.development = !is_production;
        } else {
            options.jsx.runtime = JsxRuntime::Classic;
            options.jsx.development = false;
        }

        let semantic_result = oxc::semantic::SemanticBuilder::new()
            .with_check_syntax_error(false)
            .build(&program);

        let scoping = semantic_result.semantic.into_scoping();
        let transformer = Transformer::new(&allocator, path, &options);
        let transform_result = transformer.build_with_scoping(scoping, &mut program);

        if !transform_result.diagnostics.is_empty() {
            let has_errors = transform_result.diagnostics.has_errors();
            for err in &transform_result.diagnostics {
                tracing::warn!("Transform error in {}: {:?}", file_path, err);
            }
            if has_errors {
                anyhow::bail!("Transform errors in {}", file_path);
            }
        }

        let codegen_result = Codegen::new()
            .with_options(CodegenOptions {
                minify: is_production,
                // Emit a v3 source map. Fast Refresh code is appended *after*
                // the generated code, so the mappings stay valid.
                source_map_path: Some(path.to_path_buf()),
                ..CodegenOptions::default()
            })
            .build(&program);

        let source_map = codegen_result.map.as_ref().map(|m| m.to_json_string());
        let mut code = codegen_result.code;

        // Inject Fast Refresh in dev mode using AST-based component detection
        let fast_refresh_boundaries = if !is_production {
            self.inject_fast_refresh(&mut code, file_path)
        } else {
            vec![]
        };

        Ok(ReactTransformResult {
            code,
            fast_refresh_boundaries,
            source_map,
        })
    }

    /// Inject React Fast Refresh boundary code
    /// Uses AST-level analysis to detect React component declarations
    fn inject_fast_refresh(&self, code: &mut String, _file_path: &str) -> Vec<String> {
        let boundaries = detect_react_components(code);

        if !boundaries.is_empty() {
            let component_name = boundaries.first().cloned().unwrap_or_default();
            let refresh_code = format!(
                r#"

// ─── React Fast Refresh (injected by Pledge) ───
if (import.meta.hot) {{
  import.meta.hot.accept(() => {{
    if (typeof window !== 'undefined' && window.__pledge_fast_refresh) {{
      if (typeof window.__pledge_fast_refresh === 'function') {{
        window.__pledge_fast_refresh('{}', () => import(import.meta.url));
      }} else if (window.__pledge_fast_refresh.render) {{
        window.__pledge_fast_refresh.render();
      }}
    }}
  }});
}}
"#,
                component_name
            );
            code.push_str(&refresh_code);
        }

        boundaries
    }
}

pub struct ReactTransformResult {
    pub code: String,
    pub fast_refresh_boundaries: Vec<String>,
    /// Source map (v3 JSON) for `code`, from Oxc's codegen.
    pub source_map: Option<String>,
}

fn extract_function_name(line: &str) -> Option<String> {
    let after_fn = line.strip_prefix("function ")?;
    let end = after_fn.find(|c: char| !c.is_alphanumeric() && c != '_')?;
    Some(after_fn[..end].to_string())
}

fn extract_const_name(line: &str) -> Option<String> {
    let after_const = line.strip_prefix("const ")?;
    let end = after_const.find(['=', ' ', ':'])?;
    Some(after_const[..end].trim().to_string())
}

fn is_component_name(name: &str) -> bool {
    name.chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false)
}

/// Detect React component declarations in transformed code.
/// Uses line-level analysis on the Oxc-generated output to find:
///   - function ComponentName(
///   - const ComponentName = (
///   - const ComponentName = function
///   - export default function ComponentName
fn detect_react_components(code: &str) -> Vec<String> {
    let mut components = Vec::new();
    for line in code.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("function ")
            && trimmed.contains('(')
            && !trimmed.starts_with("function use")
            && let Some(name) = extract_function_name(trimmed)
            && is_component_name(&name)
        {
            components.push(name);
        }

        if (trimmed.starts_with("const ") || trimmed.starts_with("export const "))
            && (trimmed.contains("= (") || trimmed.contains("= function"))
            && !trimmed.contains("=> {}")
            && let Some(name) = extract_const_name(trimmed)
            && is_component_name(&name)
        {
            components.push(name);
        }

        if trimmed.starts_with("export default function ")
            && let Some(name) =
                extract_function_name(trimmed.strip_prefix("export default ").unwrap_or(trimmed))
            && is_component_name(&name)
        {
            components.push(name);
        }
    }
    components
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_component_name() {
        assert!(is_component_name("App"));
        assert!(is_component_name("MyComponent"));
        assert!(!is_component_name("app"));
        assert!(!is_component_name("useHook"));
        assert!(!is_component_name("myComponent"));
    }

    #[test]
    fn test_extract_function_name() {
        assert_eq!(
            extract_function_name("function App() {"),
            Some("App".to_string())
        );
        assert_eq!(
            extract_function_name("function MyComponent(props) {"),
            Some("MyComponent".to_string())
        );
    }

    // PRODUCTION-READINESS-100.md goal 93: adapter-solid's transform() was
    // found to crash deterministically inside oxc's Codegen::build() (ropey
    // rope_builder.rs:248, Option::unwrap on None — see adapter-solid's own
    // tests). This crate's `transform()` calls `Codegen::new().with_options
    // (...).build(&program)` with the *same* options pattern, so this test
    // exists to answer: is the crash systemic to this oxc/ropey pin (would
    // affect every adapter, including this one, in real builds) or specific
    // to something Solid does (its `jsx.import_source` setting, most
    // likely)? See PRODUCTION-READINESS-100.md Phase 8 goal 93 for the
    // result and full writeup.
    #[test]
    fn transform_simple_jsx_does_not_crash_codegen() {
        let adapter = ReactAdapter::new();
        let result = adapter.transform(
            "function App() { return <div>hello</div>; }",
            pledgepack_core::module::ModuleKind::Jsx,
            "App.jsx",
            false,
        );
        assert!(result.is_ok(), "transform failed: {:?}", result.err());
        assert!(result.unwrap().code.contains("hello"));
    }

    #[test]
    fn transform_emits_a_source_map_for_the_generated_code() {
        let adapter = ReactAdapter::new();
        let result = adapter
            .transform(
                "function App() { return <div>hello</div>; }",
                pledgepack_core::module::ModuleKind::Jsx,
                "App.jsx",
                true,
            )
            .unwrap();
        let map: serde_json::Value =
            serde_json::from_str(result.source_map.as_deref().expect("no source map")).unwrap();
        assert_eq!(map["version"], 3);
        assert!(!map["mappings"].as_str().unwrap().is_empty());
        assert!(map["sources"].to_string().contains("App.jsx"));
    }
}
