// Dependency pre-bundling — scan node_modules and pre-bundle dependencies
//
// This module identifies bare imports (e.g., `import React from "react"`) in the
// project source, resolves them from node_modules, and pre-bundles them into
// optimized ESM modules. This converts CJS dependencies to ESM and deduplicates
// shared dependencies.
//
// Similar to Vite's dep pre-bundling (which uses esbuild), this module:
//   1. Scans entry points for bare imports
//   2. Resolves each dependency via the resolver
//   3. Converts CJS → ESM using interop wrappers
//   4. Writes pre-bundled deps to node_modules/.pledge-deps

use crate::config::PledgeConfig;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Pre-bundled dependency information
#[derive(Debug, Clone)]
pub struct PreBundledDep {
    /// The original bare specifier (e.g., "react")
    pub specifier: String,
    /// The resolved path in node_modules
    pub source_path: PathBuf,
    /// The pre-bundled ESM output path
    pub output_path: PathBuf,
    /// Whether this was a CJS module that needed interop
    pub was_cjs: bool,
    /// Size of the pre-bundled output in bytes
    pub size: usize,
}

/// Dep pre-bundler
pub struct DepBundler {
    deps: HashMap<String, PreBundledDep>,
}

impl DepBundler {
    pub fn new() -> Self {
        Self {
            deps: HashMap::new(),
        }
    }

    /// Scan source files for bare imports and pre-bundle them.
    /// Returns the list of pre-bundled dependencies.
    pub fn pre_bundle(&mut self, config: &PledgeConfig) -> Result<Vec<PreBundledDep>> {
        let root = &config.root;
        let deps_dir = root.join("node_modules").join(".pledge-deps");
        std::fs::create_dir_all(&deps_dir)?;
        let node_modules = root
            .join("node_modules")
            .canonicalize()
            .unwrap_or_else(|_| root.join("node_modules"));
        let resolver = crate::engine::module_resolver(config);
        // Any path inside the root works as the importer for bare specifiers.
        let importer = root.join("index.js");

        // Step 1: Scan all entry points and their dependencies for bare imports
        let bare_imports = self.scan_for_bare_imports(config)?;

        info!("Pre-bundling {} dependencies", bare_imports.len());

        // Step 2: Resolve and pre-bundle each dependency
        for specifier in &bare_imports {
            if self.deps.contains_key(specifier) {
                continue;
            }

            match self.pre_bundle_dep(
                specifier,
                root,
                &node_modules,
                &deps_dir,
                &resolver,
                &importer,
            ) {
                Ok(dep) => {
                    info!(
                        "  ✓ {} ({} bytes{})",
                        dep.specifier,
                        dep.size,
                        if dep.was_cjs { " [CJS→ESM]" } else { "" }
                    );
                    self.deps.insert(specifier.clone(), dep);
                }
                Err(e) => {
                    warn!("  ✗ Failed to pre-bundle {}: {}", specifier, e);
                }
            }
        }

        Ok(self.deps.values().cloned().collect())
    }

    /// File name a specifier is pre-bundled to inside `.pledge-deps`
    /// (`react-dom/client` → `react-dom_client.js`, `@org/pkg` → `org_pkg.js`).
    pub fn dep_file_name(specifier: &str) -> String {
        format!("{}.js", specifier.replace('/', "_").replace('@', ""))
    }

    /// URL path a pre-bundled specifier is served at (`/node_modules/.pledge-deps/<file>`).
    pub fn dep_url(specifier: &str) -> String {
        format!(
            "/node_modules/.pledge-deps/{}",
            Self::dep_file_name(specifier)
        )
    }

    /// Scan source files for bare imports (non-relative specifiers)
    fn scan_for_bare_imports(&self, config: &PledgeConfig) -> Result<HashSet<String>> {
        let mut imports = HashSet::new();

        for entry in &config.entry {
            let entry_path = config.root.join(entry);
            if entry_path.exists() {
                let source = std::fs::read_to_string(&entry_path)?;
                Self::extract_bare_imports(&source, &mut imports);
            }
        }

        // Also scan common source directories
        let src_dir = config.root.join("src");
        if src_dir.exists() {
            self.scan_directory(&src_dir, &mut imports)?;
        }

        Ok(imports)
    }

    /// Recursively scan a directory for bare imports
    fn scan_directory(&self, dir: &PathBuf, imports: &mut HashSet<String>) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                // Skip node_modules and hidden directories
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n == "node_modules" || n.starts_with('.'))
                    .unwrap_or(false)
                {
                    continue;
                }
                self.scan_directory(&path, imports)?;
            } else if path.is_file() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if matches!(ext, "ts" | "tsx" | "js" | "jsx" | "mjs")
                    && let Ok(source) = std::fs::read_to_string(&path)
                {
                    Self::extract_bare_imports(&source, imports);
                }
            }
        }

        Ok(())
    }

    /// Extract bare import specifiers from source code
    fn extract_bare_imports(source: &str, imports: &mut HashSet<String>) {
        // Look for import patterns: from "X", import "X", import("X")
        for pattern in ["from \"", "from '", "import \"", "import '", "import("] {
            let mut search_pos = 0;
            while let Some(pos) = source[search_pos..].find(pattern) {
                let abs_pos = search_pos + pos;
                let after_pattern = abs_pos + pattern.len();
                let rest = &source[after_pattern..];

                let closing = if pattern.ends_with('"') {
                    '"'
                } else if pattern.ends_with('\'') {
                    '\''
                } else {
                    '('
                };

                if closing == '(' {
                    // Dynamic import: find the string inside
                    if let Some(q_pos) = rest.find(['"', '\'']) {
                        let q_char = rest.as_bytes()[q_pos] as char;
                        let spec_start = q_pos + 1;
                        if let Some(end) = rest[spec_start..].find(q_char) {
                            let specifier = &rest[spec_start..spec_start + end];
                            if Self::is_bare_specifier(specifier) {
                                imports.insert(specifier.to_string());
                            }
                        }
                    }
                } else if let Some(end) = rest.find(closing) {
                    let specifier = &rest[..end];
                    if Self::is_bare_specifier(specifier) {
                        imports.insert(specifier.to_string());
                    }
                }

                search_pos = after_pattern;
            }
        }
    }

    /// Check if a specifier is a bare import (not relative or absolute)
    fn is_bare_specifier(specifier: &str) -> bool {
        !specifier.starts_with("./")
            && !specifier.starts_with("../")
            && !specifier.starts_with("/")
            && !specifier.starts_with("http")
            && !specifier.is_empty()
    }

    /// Pre-bundle a single dependency
    fn pre_bundle_dep(
        &self,
        specifier: &str,
        _root: &Path,
        node_modules: &Path,
        deps_dir: &Path,
        resolver: &pledgepack_resolver::Resolver,
        importer: &Path,
    ) -> Result<PreBundledDep> {
        // Resolve through the shared resolver: exports conditions, subpaths,
        // workspace packages and pnpm layouts all come for free.
        let dep_path = resolver
            .resolve(specifier, importer)
            .map_err(|e| anyhow::anyhow!("could not resolve dependency {specifier}: {e}"))?;

        // Read the source
        let source = std::fs::read_to_string(&dep_path)?;

        // Check if it's CJS or ESM
        let is_cjs = !source.contains("export ")
            && !source.contains("export default")
            && !source.contains("import ")
            && (source.contains("module.exports") || source.contains("require("));

        // CJS deps get a self-contained interop wrapper. ESM deps get a
        // re-export shim pointing at the canonical node_modules URL — a raw
        // copy into .pledge-deps would break the dep's own relative imports.
        let (esm_code, was_cjs) = if is_cjs {
            (Self::cjs_to_esm_wrapper(specifier, &source), true)
        } else {
            (
                Self::esm_shim(specifier, &dep_path, node_modules, &source),
                false,
            )
        };

        // Write pre-bundled output
        let output_path = deps_dir.join(Self::dep_file_name(specifier));
        std::fs::write(&output_path, &esm_code)?;

        let size = esm_code.len();

        Ok(PreBundledDep {
            specifier: specifier.to_string(),
            source_path: dep_path,
            output_path,
            was_cjs,
            size,
        })
    }

    /// URL path a module file is served at: `/node_modules/<rel>` when it
    /// lives under the canonical node_modules dir, `/@fs/<abs>` otherwise
    /// (e.g. a workspace package outside the project root).
    fn dep_web_url(dep_path: &Path, node_modules: &Path) -> String {
        if let Ok(rel) = dep_path.strip_prefix(node_modules) {
            format!("/node_modules/{}", crate::normalize_path(rel))
        } else {
            format!("/@fs/{}", crate::normalize_path(dep_path))
        }
    }

    /// An ESM shim module: re-exports everything (and the default, when the
    /// source declares one) from the dep's canonical served URL.
    fn esm_shim(specifier: &str, dep_path: &Path, node_modules: &Path, source: &str) -> String {
        let url = Self::dep_web_url(dep_path, node_modules);
        let url_js = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string());
        let mut shim =
            format!("// Pledge pre-bundled: {specifier} (ESM)\nexport * from {url_js};\n");
        if source.contains("export default") {
            shim.push_str(&format!("export {{ default }} from {url_js};\n"));
        }
        shim
    }

    /// Generate an ESM wrapper for a CJS module
    pub fn cjs_to_esm_wrapper(specifier: &str, cjs_source: &str) -> String {
        // Create an ESM wrapper that imports the CJS module and re-exports
        let _safe_name = specifier
            .replace('/', "_")
            .replace('@', "")
            .replace('-', "_");

        format!(
            r#"// Pledge pre-bundled: {} (CJS → ESM interop)
const __pledge_cjs_module = {{}};
const __pledge_require = (id) => __pledge_cjs_module.exports || {{}};
const module = {{ exports: __pledge_cjs_module }};
const exports = __pledge_cjs_module;
const require = __pledge_require;

{}
const __pledge_default = module.exports;

export default __pledge_default;
export const __pledge_named = new Proxy(__pledge_default, {{
    get: (target, prop) => target[prop]
}});
"#,
            specifier, cjs_source
        )
    }

    /// Get the pre-bundled dependency info for a specifier
    pub fn get_dep(&self, specifier: &str) -> Option<&PreBundledDep> {
        self.deps.get(specifier)
    }

    /// Get all pre-bundled dependencies
    pub fn deps(&self) -> &HashMap<String, PreBundledDep> {
        &self.deps
    }
}

impl Default for DepBundler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_bare_specifier() {
        assert!(DepBundler::is_bare_specifier("react"));
        assert!(DepBundler::is_bare_specifier("@tanstack/router"));
        assert!(!DepBundler::is_bare_specifier("./foo"));
        assert!(!DepBundler::is_bare_specifier("../bar"));
        assert!(!DepBundler::is_bare_specifier("/abs/path"));
    }

    #[test]
    fn test_extract_bare_imports() {
        let source = r#"
            import React from "react";
            import { createRoot } from "react-dom/client";
            import { defineConfig } from "pledgepack";
            import "./local.css";
            import "../utils.js";
        "#;
        let mut imports = HashSet::new();
        DepBundler::extract_bare_imports(source, &mut imports);
        assert!(imports.contains("react"));
        assert!(imports.contains("react-dom/client"));
        assert!(imports.contains("pledgepack"));
        assert!(!imports.contains("./local.css"));
        assert!(!imports.contains("../utils.js"));
    }
}
