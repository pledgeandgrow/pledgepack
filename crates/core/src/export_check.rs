//! Static `import { x } from './mod'` validation.
//!
//! Rollup fails a build with `"zzz" is not exported by "src/utils.ts"`; a
//! bundler that silently emits `undefined` for that import hides real bugs.
//! This pass compares each module's named / default imports against the
//! exports of the (project-source) module they resolve to.
//!
//! Only ES modules whose export list is statically knowable are checked: a
//! target that uses CommonJS (`module.exports`, no ESM syntax), `export *`
//! (its names come from other modules), `export =`, or that is not
//! JS/TS, is skipped rather than guessed at.

use crate::diagnostics::{SourceDiagnostic, diagnostic_at, display_file};
use oxc::allocator::Allocator;
use oxc::ast::ast::{
    BindingPattern, Declaration, ExportDefaultDeclarationKind, ImportDeclarationSpecifier,
    ImportOrExportKind, ModuleExportName, Statement,
};
use oxc::parser::Parser;
use oxc::span::SourceType;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One imported binding to verify.
#[derive(Debug, Clone)]
pub struct ImportedName {
    pub specifier: String,
    /// `"default"` for a default import, otherwise the exported name.
    pub name: String,
    pub start: usize,
    pub len: usize,
}

/// The statically-known export surface of a module.
#[derive(Debug, Clone, Default)]
pub struct ExportInfo {
    pub names: HashSet<String>,
    pub has_default: bool,
    /// `false` when the export list cannot be determined statically
    /// (CommonJS, `export *`, `export =`, no module syntax at all).
    pub analyzable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleInfo {
    pub imports: Vec<ImportedName>,
    pub exports: ExportInfo,
}

/// Whether the export check applies to this file type.
pub fn is_checkable(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "mjs" | "jsx" | "ts" | "mts" | "tsx")
    ) && !path.components().any(|c| c.as_os_str() == "node_modules")
}

fn export_name(n: &ModuleExportName<'_>) -> String {
    n.name().to_string()
}

fn binding_names(p: &BindingPattern<'_>, out: &mut Vec<String>) {
    match p {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPattern::ObjectPattern(o) => {
            for prop in &o.properties {
                binding_names(&prop.value, out);
            }
            if let Some(rest) = &o.rest {
                binding_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(a) => {
            for el in a.elements.iter().flatten() {
                binding_names(el, out);
            }
            if let Some(rest) = &a.rest {
                binding_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(a) => binding_names(&a.left, out),
    }
}

/// Parse `source` and collect its imports and exports. `None` when the file
/// does not parse (the transform step reports that error properly).
pub fn analyze_module(source: &str, path: &Path) -> Option<ModuleInfo> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let ret = Parser::new(&allocator, source, source_type).parse();
    if ret.panicked || !ret.diagnostics.is_empty() {
        return None;
    }

    let mut info = ModuleInfo::default();
    let mut is_esm = false;
    let mut analyzable = true;

    for stmt in &ret.program.body {
        match stmt {
            Statement::ImportDeclaration(imp) => {
                is_esm = true;
                if imp.import_kind == ImportOrExportKind::Type {
                    continue;
                }
                let spec = imp.source.value.to_string();
                for s in imp.specifiers.iter().flatten() {
                    match s {
                        ImportDeclarationSpecifier::ImportSpecifier(is) => {
                            if is.import_kind == ImportOrExportKind::Type {
                                continue;
                            }
                            info.imports.push(ImportedName {
                                specifier: spec.clone(),
                                name: export_name(&is.imported),
                                start: is.span.start as usize,
                                len: (is.span.end - is.span.start) as usize,
                            });
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(d) => {
                            info.imports.push(ImportedName {
                                specifier: spec.clone(),
                                name: "default".to_string(),
                                start: d.span.start as usize,
                                len: (d.span.end - d.span.start) as usize,
                            });
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {}
                    }
                }
            }
            Statement::ExportDefaultDeclaration(d) => {
                is_esm = true;
                info.exports.has_default = true;
                if let ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) = d.declaration {
                    // `export default interface` is type-only but still a default export.
                }
            }
            Statement::ExportAllDeclaration(all) => {
                is_esm = true;
                match &all.exported {
                    Some(name) => {
                        info.exports.names.insert(export_name(name));
                    }
                    None => analyzable = false,
                }
            }
            Statement::ExportNamedDeclaration(exp) => {
                is_esm = true;
                if let Some(decl) = &exp.declaration {
                    match decl {
                        Declaration::VariableDeclaration(v) => {
                            let mut names = Vec::new();
                            for d in &v.declarations {
                                binding_names(&d.id, &mut names);
                            }
                            info.exports.names.extend(names);
                        }
                        Declaration::FunctionDeclaration(f) => {
                            if let Some(id) = &f.id {
                                info.exports.names.insert(id.name.to_string());
                            }
                        }
                        Declaration::ClassDeclaration(c) => {
                            if let Some(id) = &c.id {
                                info.exports.names.insert(id.name.to_string());
                            }
                        }
                        Declaration::TSTypeAliasDeclaration(t) => {
                            info.exports.names.insert(t.id.name.to_string());
                        }
                        Declaration::TSInterfaceDeclaration(t) => {
                            info.exports.names.insert(t.id.name.to_string());
                        }
                        Declaration::TSEnumDeclaration(t) => {
                            info.exports.names.insert(t.id.name.to_string());
                        }
                        // Namespaces, `declare module`, import aliases: not
                        // enumerable here.
                        _ => analyzable = false,
                    }
                }
                for s in &exp.specifiers {
                    let name = export_name(&s.exported);
                    if name == "default" {
                        info.exports.has_default = true;
                    } else {
                        info.exports.names.insert(name);
                    }
                    // `export { a } from './x'` also imports `a` from './x'.
                    if let Some(src) = &exp.source
                        && exp.export_kind != ImportOrExportKind::Type
                        && s.export_kind != ImportOrExportKind::Type
                    {
                        info.imports.push(ImportedName {
                            specifier: src.value.to_string(),
                            name: export_name(&s.local),
                            start: s.span.start as usize,
                            len: (s.span.end - s.span.start) as usize,
                        });
                    }
                }
            }
            // `export = x`, `export as namespace X`, `import x = require()`.
            Statement::TSExportAssignment(_) | Statement::TSNamespaceExportDeclaration(_) => {
                analyzable = false;
            }
            _ => {}
        }
    }

    // No ESM syntax at all => CommonJS (or a script): cannot enumerate.
    info.exports.analyzable = analyzable && is_esm;
    Some(info)
}

/// Verify imports across `modules` (path -> source). `resolve` maps
/// `(specifier, importer)` to a module path (or `None` for externals /
/// unresolved). Returns one diagnostic per bad import.
pub fn check_imports(
    modules: &[(PathBuf, String)],
    root: &Path,
    resolve: &mut dyn FnMut(&str, &Path) -> Option<PathBuf>,
) -> Vec<SourceDiagnostic> {
    let sources: HashMap<&Path, &str> = modules
        .iter()
        .map(|(p, s)| (p.as_path(), s.as_str()))
        .collect();
    let infos: HashMap<&Path, ModuleInfo> = modules
        .iter()
        .filter(|(p, _)| is_checkable(p))
        .filter_map(|(p, s)| analyze_module(s, p).map(|i| (p.as_path(), i)))
        .collect();

    let mut ordered: Vec<&Path> = infos.keys().copied().collect();
    ordered.sort();

    let mut out = Vec::new();
    for importer in ordered {
        let info = &infos[importer];
        for imp in &info.imports {
            let Some(target) = resolve(&imp.specifier, importer) else {
                continue;
            };
            let Some(target_info) = infos.get(target.as_path()) else {
                continue;
            };
            if !target_info.exports.analyzable {
                continue;
            }
            let present = if imp.name == "default" {
                target_info.exports.has_default
            } else {
                target_info.exports.names.contains(&imp.name)
            };
            if present {
                continue;
            }
            let target_display = display_file(&target.to_string_lossy(), root);
            let importer_display = display_file(&importer.to_string_lossy(), root);
            let message = if imp.name == "default" {
                format!(
                    "\"default\" is not exported by \"{}\", imported by \"{}\"",
                    target_display, importer_display
                )
            } else {
                format!(
                    "\"{}\" is not exported by \"{}\", imported by \"{}\"",
                    imp.name, target_display, importer_display
                )
            };
            let help = if imp.name == "default" {
                Some(format!(
                    "\"{}\" has no default export; use a named import or add `export default`",
                    target_display
                ))
            } else {
                let names: Vec<&str> = target_info
                    .exports
                    .names
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                crate::config_validate::find_closest_match(&imp.name, &names)
                    .map(|s| format!("did you mean \"{}\"?", s))
            };
            let src = sources.get(importer).copied().unwrap_or("");
            out.push(diagnostic_at(
                &importer_display,
                src,
                imp.start,
                imp.len,
                message,
                help,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(files: &[(&str, &str)]) -> Vec<String> {
        let modules: Vec<(PathBuf, String)> = files
            .iter()
            .map(|(p, s)| (PathBuf::from(format!("/proj/{}", p)), s.to_string()))
            .collect();
        let mut resolve = |spec: &str, importer: &Path| -> Option<PathBuf> {
            if !spec.starts_with('.') {
                return None;
            }
            let base = importer.parent()?.join(spec.trim_start_matches("./"));
            for ext in ["", ".ts", ".tsx", ".js", ".jsx"] {
                let cand = PathBuf::from(format!("{}{}", base.display(), ext));
                if modules.iter().any(|(p, _)| *p == cand) {
                    return Some(cand);
                }
            }
            None
        };
        check_imports(&modules, Path::new("/proj"), &mut resolve)
            .into_iter()
            .map(|d| d.to_string())
            .collect()
    }

    #[test]
    fn reports_missing_named_export_with_frame() {
        let errs = run(&[
            (
                "src/index.ts",
                "import { greet, zzz } from './utils';\nconsole.log(greet, zzz);\n",
            ),
            ("src/utils.ts", "export function greet() {}\n"),
        ]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("\"zzz\" is not exported by \"src/utils.ts\""),
            "{}",
            errs[0]
        );
        assert!(errs[0].contains("imported by \"src/index.ts\""));
        assert!(errs[0].contains("--> src/index.ts:1:17"), "{}", errs[0]);
    }

    #[test]
    fn reports_missing_default_but_accepts_present_ones() {
        let errs = run(&[
            (
                "src/a.ts",
                "import x from './b';\nimport y from './c';\nimport { t } from './b';\n",
            ),
            ("src/b.ts", "export const t = 1;\nexport type U = string;\n"),
            ("src/c.ts", "const v = 1;\nexport default v;\n"),
        ]);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("\"default\" is not exported by \"src/b.ts\""));
    }

    #[test]
    fn skips_commonjs_star_reexports_and_type_imports() {
        let errs = run(&[
            (
                "src/a.ts",
                "import { a } from './cjs';\nimport { b } from './star';\nimport type { Nope } from './typed';\nimport { type Nope2, ok } from './typed';\n",
            ),
            ("src/cjs.js", "module.exports = { a: 1 };\n"),
            ("src/star.ts", "export * from './other';\n"),
            ("src/typed.ts", "export const ok = 1;\n"),
        ]);
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn reexports_and_destructured_exports_count() {
        let errs = run(&[
            (
                "src/a.ts",
                "import { p, q, r, s } from './m';\nconsole.log(p, q, r, s);\n",
            ),
            (
                "src/m.ts",
                "const x = { p: 1, q: 2 };\nexport const { p, q } = x;\nconst r = 1;\nfunction s() {}\nexport { r, s };\n",
            ),
        ]);
        assert!(errs.is_empty(), "{errs:?}");
    }
}
