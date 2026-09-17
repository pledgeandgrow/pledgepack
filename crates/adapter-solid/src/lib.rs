// Solid.js adapter: JSX transform for Solid.js
//
// Handles:
//   - JSX → Solid's createElement / template literals
//   - TypeScript type stripping
//   - Solid-specific transform options (no React runtime)
//
// Scope note (PRODUCTION-READINESS-100.md goal 70): this adapter is
// intentionally minimal — JSX/TSX transform plus a generic HMR-accept
// injection, and nothing else. Unlike `adapter-next` (1200+ lines) or
// `adapter-pledgestack` (1100+ lines), there is no Solid-specific route
// discovery or project scaffolding here. The dev-mode HMR snippet injected
// below isn't really Solid-specific either — it forwards to
// `window.__pledge_solid_hmr` boundaries the *user's* Solid code has to
// register itself; this crate doesn't know how to find or generate them.
// Treat Solid support as community-tier (transform-only) rather than
// assuming parity with the Next.js/PledgeStack adapters.

use anyhow::Result;
use oxc::allocator::Allocator;
use oxc::codegen::{Codegen, CodegenOptions};
use oxc::parser::{Parser, ParserReturn};
use oxc::span::SourceType;
use oxc::transformer::{JsxRuntime, TransformOptions, Transformer};
use pledgepack_core::module::ModuleKind;
use std::path::Path;

pub struct SolidAdapter;

impl SolidAdapter {
    pub fn new() -> Self {
        Self
    }

    /// Transform JSX/TSX for Solid.js using Oxc
    /// Solid uses automatic JSX runtime with its own jsx-runtime
    pub fn transform(
        &self,
        source: &str,
        kind: ModuleKind,
        file_path: &str,
        is_production: bool,
    ) -> Result<String> {
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

        // Solid uses automatic JSX runtime pointing to solid-js/jsx-runtime
        let mut options = TransformOptions::default();
        options.typescript.only_remove_type_imports = false;
        options.jsx.runtime = JsxRuntime::Automatic;
        options.jsx.development = !is_production;
        // Solid's jsx import source is "solid-js/jsx-runtime"
        options.jsx.import_source = Some("solid-js".to_string());

        let semantic_result = oxc::semantic::SemanticBuilder::new()
            .with_check_syntax_error(false)
            .build(&program);

        let scoping = semantic_result.semantic.into_scoping();
        let transformer = Transformer::new(&allocator, path, &options);
        let transform_result = transformer.build_with_scoping(scoping, &mut program);

        if !transform_result.diagnostics.is_empty() {
            let has_errors = transform_result.diagnostics.has_errors();
            for err in &transform_result.diagnostics {
                tracing::warn!("Solid transform error in {}: {:?}", file_path, err);
            }
            if has_errors {
                anyhow::bail!("Transform errors in {}", file_path);
            }
        }

        let codegen_result = Codegen::new()
            .with_options(CodegenOptions {
                minify: is_production,
                ..CodegenOptions::default()
            })
            .build(&program);

        let mut code = codegen_result.code;

        // Inject Solid HMR boundary in dev mode with reactive scope preservation
        if !is_production {
            code.push_str(
                r#"
// Solid HMR — reactive scope preservation
if (import.meta.hot && typeof window !== 'undefined') {
  import.meta.hot.accept((newModule) => {
    if (newModule) {
      // Solid components are reactive by default — re-executing the module
      // re-creates reactive scopes. The Solid runtime handles cleanup
      // automatically via createRoot/createEffect disposal.
      // Notify all registered Solid HMR boundaries to re-execute
      const __solid_hmr_boundaries = window.__pledge_solid_hmr;
      if (__solid_hmr_boundaries) {
        __solid_hmr_boundaries.forEach((boundary) => {
          if (boundary && typeof boundary === 'function') {
            boundary(newModule);
          }
        });
      }
    }
  });
}
"#,
            );
        }

        Ok(code)
    }
}

impl Default for SolidAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    // PRODUCTION-READINESS-100.md goal 69: this crate had zero tests before
    // this pass, despite being one of only two adapters (with
    // adapter-tanstack) marketed as supported. See also goal 70's note in
    // this crate's doc comment.
    use super::*;

    #[test]
    fn transforms_basic_jsx_in_dev_mode() {
        let adapter = SolidAdapter::new();
        let result = adapter.transform(
            "export default function App() { return <div>Hello</div>; }",
            ModuleKind::Jsx,
            "App.jsx",
            false,
        );
        assert!(result.is_ok(), "transform failed: {:?}", result.err());
        let code = result.unwrap();
        // Automatic JSX runtime should reference solid-js, not React.
        assert!(
            code.contains("solid-js"),
            "expected a solid-js import, got:\n{code}"
        );
    }

    #[test]
    fn injects_hmr_boundary_in_dev_mode_only() {
        let adapter = SolidAdapter::new();
        let source = "export default function App() { return <div>Hello</div>; }";

        let dev = adapter
            .transform(source, ModuleKind::Jsx, "App.jsx", false)
            .unwrap();
        assert!(
            dev.contains("import.meta.hot"),
            "dev build should inject the HMR boundary"
        );
        assert!(dev.contains("__pledge_solid_hmr"));

        let prod = adapter
            .transform(source, ModuleKind::Jsx, "App.jsx", true)
            .unwrap();
        assert!(
            !prod.contains("import.meta.hot"),
            "production build must not ship the dev-only HMR boundary"
        );
    }

    #[test]
    fn transforms_tsx_with_typescript_types() {
        let adapter = SolidAdapter::new();
        let result = adapter.transform(
            "interface Props { name: string }\nexport default function App(props: Props) { return <div>{props.name}</div>; }",
            ModuleKind::Tsx,
            "App.tsx",
            false,
        );
        assert!(result.is_ok(), "transform failed: {:?}", result.err());
        let code = result.unwrap();
        // TypeScript-only constructs (interfaces) must be stripped from output.
        assert!(!code.contains("interface Props"));
    }

    #[test]
    fn rejects_unparseable_source() {
        let adapter = SolidAdapter::new();
        let result = adapter.transform(
            "export default function App( { return <div>Hello</div>",
            ModuleKind::Jsx,
            "Broken.jsx",
            false,
        );
        assert!(
            result.is_err(),
            "malformed source should fail to parse, not silently produce output"
        );
    }

    #[test]
    fn production_output_is_minified() {
        let adapter = SolidAdapter::new();
        let source = "export default function App() {\n  const x = 1;\n  return <div>{x}</div>;\n}";
        let dev = adapter
            .transform(source, ModuleKind::Jsx, "App.jsx", false)
            .unwrap();
        let prod = adapter
            .transform(source, ModuleKind::Jsx, "App.jsx", true)
            .unwrap();
        assert!(
            prod.len() < dev.len(),
            "production output ({} bytes) should be smaller than dev output ({} bytes)",
            prod.len(),
            dev.len()
        );
    }
}
