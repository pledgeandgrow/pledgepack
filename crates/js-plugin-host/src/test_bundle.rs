//! Bundle a test file together with the project modules it imports.
//!
//! `pledge test` evaluates test files in QuickJS, which has no module loader.
//! Previously every `import` line was deleted, so
//! `import { greet } from './utils'` left `greet` undefined. This module
//! instead:
//!
//! 1. transforms each module with the same Oxc pipeline the build uses (TS,
//!    JSX, ...),
//! 2. resolves every import with the build engine's resolver (aliases,
//!    tsconfig paths, `node_modules`, extensions),
//! 3. rewrites ES module syntax into a small CommonJS-style wrapper
//!    (imports become `require`s, exports become live getters), and
//! 4. emits one script that registers all modules and runs the entry.
//!
//! `vitest` / `@jest/globals` imports resolve to the harness globals
//! (`describe`, `it`, `expect`, `vi`, hooks), and Node built-ins resolve to
//! an empty object. An import that cannot be resolved throws only when it is
//! actually executed, so unused imports never break a run.

use anyhow::{Result, anyhow};
use oxc::allocator::Allocator;
use oxc::ast::ast::{
    BindingPattern, Declaration, ExportDefaultDeclarationKind, Expression,
    ImportDeclarationSpecifier, ModuleExportName, Statement,
};
use oxc::ast_visit::Visit;
use oxc::parser::Parser;
use oxc::span::{GetSpan, SourceType};
use pledgepack_core::module::ModuleKind;
use pledgepack_core::{BuildEngine, PledgeConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Module ids `>= 0` index into the bundle; these are the two virtual targets.
const DEP_HARNESS: i64 = -1;
const DEP_BUILTIN: i64 = -2;

const HARNESS_SPECIFIERS: &[&str] = &["vitest", "@jest/globals", "@vitest/globals", "node:test"];

const NODE_BUILTINS: &[&str] = &[
    "assert",
    "buffer",
    "child_process",
    "crypto",
    "events",
    "fs",
    "http",
    "https",
    "net",
    "os",
    "path",
    "stream",
    "url",
    "util",
    "zlib",
    "readline",
    "module",
    "process",
    "timers",
    "tty",
    "worker_threads",
    "perf_hooks",
    "querystring",
    "string_decoder",
    "vm",
    "dns",
    "cluster",
];

fn is_builtin(spec: &str) -> bool {
    let bare = spec.strip_prefix("node:").unwrap_or(spec);
    let head = bare.split('/').next().unwrap_or(bare);
    spec.starts_with("node:") || NODE_BUILTINS.contains(&head)
}

struct Compiled {
    path: PathBuf,
    /// JS body of the module wrapper.
    body: String,
    /// specifier -> dependency id (>= 0 module, or a `DEP_*` constant).
    deps: HashMap<String, i64>,
}

/// Bundle `entry` and everything it imports into a single script.
pub fn bundle_test_file(entry: &Path, project_config: &PledgeConfig) -> Result<String> {
    let mut cfg = project_config.clone();
    cfg.cache.enabled = false;
    let cfg = Arc::new(cfg);
    let engine = BuildEngine::new(cfg.clone());

    let mut ids: HashMap<PathBuf, i64> = HashMap::new();
    let mut compiled: Vec<Option<Compiled>> = Vec::new();
    let mut queue: Vec<PathBuf> = Vec::new();

    let entry = entry.to_path_buf();
    ids.insert(entry.clone(), 0);
    compiled.push(None);
    queue.push(entry.clone());

    while let Some(path) = queue.pop() {
        let id = ids[&path] as usize;
        let (body, specs) = compile_module(&path, &cfg)?;
        let mut deps = HashMap::new();
        for spec in specs {
            if deps.contains_key(&spec) {
                continue;
            }
            if HARNESS_SPECIFIERS.contains(&spec.as_str()) {
                deps.insert(spec, DEP_HARNESS);
                continue;
            }
            if is_builtin(&spec) {
                deps.insert(spec, DEP_BUILTIN);
                continue;
            }
            // Unresolved specifiers are left out of the map: `require` throws
            // "Cannot find module" only if the import is actually executed.
            if let Ok(resolved) = engine.resolve_specifier(&spec, &path) {
                let dep_id = match ids.get(&resolved) {
                    Some(i) => *i,
                    None => {
                        let i = compiled.len() as i64;
                        ids.insert(resolved.clone(), i);
                        compiled.push(None);
                        queue.push(resolved);
                        i
                    }
                };
                deps.insert(spec, dep_id);
            }
        }
        compiled[id] = Some(Compiled { path, body, deps });
    }

    let mut out = String::from(PRELUDE);
    for (id, module) in compiled.iter().enumerate() {
        let Some(m) = module else { continue };
        let deps_json = serde_json::to_string(&m.deps)?;
        out.push_str(&format!(
            "__defs[{id}] = function (module, exports, require) {{\n{body}\n}};\n__deps[{id}] = {deps_json};\n__names[{id}] = {name};\n",
            id = id,
            body = m.body,
            deps_json = deps_json,
            name = serde_json::to_string(&m.path.to_string_lossy().replace('\\', "/"))?,
        ));
    }
    out.push_str("__load(0);\n})();\n");
    Ok(out)
}

const PRELUDE: &str = r#"(function () {
var __defs = {}, __deps = {}, __names = {}, __cache = {};
function __harness() {
  var g = globalThis;
  return {
    describe: g.describe, it: g.it, test: g.test, suite: g.describe, expect: g.expect, vi: g.vi,
    beforeAll: g.beforeAll, beforeEach: g.beforeEach, afterAll: g.afterAll, afterEach: g.afterEach,
    assert: g.assert, expectTypeOf: g.expectTypeOf
  };
}
function __load(id) {
  var cached = __cache[id];
  if (cached) return cached.exports;
  var mod = { exports: {} };
  __cache[id] = mod;
  __defs[id].call(mod.exports, mod, mod.exports, function (spec) {
    var dep = __deps[id][spec];
    if (dep === undefined) throw new Error("Cannot find module '" + spec + "' imported from " + __names[id]);
    if (dep === -1) return __harness();
    if (dep === -2) return {};
    return __load(dep);
  });
  return mod.exports;
}
function __default(m) { return m && m.__esModule ? m.default : m; }
function __reexport(target, m) {
  Object.keys(m).forEach(function (k) {
    if (k === "default" || k === "__esModule" || Object.prototype.hasOwnProperty.call(target, k)) return;
    Object.defineProperty(target, k, { enumerable: true, get: function () { return m[k]; } });
  });
}
function __export(target, name, get) {
  Object.defineProperty(target, name, { enumerable: true, configurable: true, get: get });
}
"#;

fn compile_module(path: &Path, config: &PledgeConfig) -> Result<(String, Vec<String>)> {
    let kind = ModuleKind::from_path(path);
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("cannot read {}: {}", path.display(), e))?;
    match kind {
        ModuleKind::Json => Ok((format!("module.exports = {};", text.trim()), Vec::new())),
        ModuleKind::JavaScript
        | ModuleKind::TypeScript
        | ModuleKind::Jsx
        | ModuleKind::Tsx
        | ModuleKind::Worker
        | ModuleKind::WebComponent => {
            let out = pledgepack_core::transform::transform(
                &text,
                kind,
                &pledgepack_core::normalize_path(path),
                false,
                config,
            )?;
            rewrite_module(&out.code)
        }
        // Styles, images and other assets have no runtime value in a test.
        _ => Ok((
            "module.exports = { __esModule: true };".to_string(),
            Vec::new(),
        )),
    }
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

fn export_name(n: &ModuleExportName<'_>) -> String {
    n.name().to_string()
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Collects `require('x')` calls and `import('x')` expressions with literal
/// specifiers (dynamic imports are rewritten to resolved promises).
struct DepScanner {
    specs: Vec<String>,
    /// (start, end, replacement)
    edits: Vec<(usize, usize, String)>,
}

impl<'a> Visit<'a> for DepScanner {
    fn visit_call_expression(&mut self, call: &oxc::ast::ast::CallExpression<'a>) {
        if let Expression::Identifier(id) = &call.callee
            && id.name == "require"
            && call.arguments.len() == 1
            && let Some(Expression::StringLiteral(s)) = call.arguments[0].as_expression()
        {
            self.specs.push(s.value.to_string());
        }
        oxc::ast_visit::walk::walk_call_expression(self, call);
    }

    fn visit_import_expression(&mut self, expr: &oxc::ast::ast::ImportExpression<'a>) {
        if let Expression::StringLiteral(s) = &expr.source {
            self.specs.push(s.value.to_string());
            self.edits.push((
                expr.span.start as usize,
                expr.span.end as usize,
                format!(
                    "Promise.resolve().then(function () {{ return require({}); }})",
                    js_str(&s.value)
                ),
            ));
        }
        oxc::ast_visit::walk::walk_import_expression(self, expr);
    }
}

/// Rewrite transformed (ESM) JavaScript into a CommonJS-style module body.
/// Returns the body and every specifier it depends on.
fn rewrite_module(code: &str) -> Result<(String, Vec<String>)> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if ret.panicked || !ret.diagnostics.is_empty() {
        let msg = ret
            .diagnostics
            .iter()
            .next()
            .map(|d| d.message.to_string())
            .unwrap_or_else(|| "parse error".to_string());
        return Err(anyhow!(
            "error: cannot bundle transformed test module: {}",
            msg
        ));
    }

    let mut specs: Vec<String> = Vec::new();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    // (exported name, JS expression evaluating to the value)
    let mut exports: Vec<(String, String)> = Vec::new();
    let mut counter = 0usize;
    let mut is_esm = false;
    let fresh = |counter: &mut usize| {
        let n = format!("__m{}", *counter);
        *counter += 1;
        n
    };

    for stmt in &ret.program.body {
        match stmt {
            Statement::ImportDeclaration(imp) => {
                is_esm = true;
                let spec = imp.source.value.to_string();
                specs.push(spec.clone());
                let tmp = fresh(&mut counter);
                let mut text = format!("var {} = require({});", tmp, js_str(&spec));
                for s in imp.specifiers.iter().flatten() {
                    match s {
                        ImportDeclarationSpecifier::ImportSpecifier(is) => text.push_str(&format!(
                            " var {} = {}[{}];",
                            is.local.name,
                            tmp,
                            js_str(&export_name(&is.imported))
                        )),
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(d) => {
                            text.push_str(&format!(" var {} = __default({});", d.local.name, tmp))
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(n) => {
                            text.push_str(&format!(" var {} = {};", n.local.name, tmp))
                        }
                    }
                }
                edits.push((imp.span.start as usize, imp.span.end as usize, text));
            }
            Statement::ExportDefaultDeclaration(d) => {
                is_esm = true;
                let start = d.span.start as usize;
                let decl_start = d.declaration.span().start as usize;
                let named = match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        f.id.as_ref().map(|i| i.name.to_string())
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                        c.id.as_ref().map(|i| i.name.to_string())
                    }
                    _ => None,
                };
                match named {
                    Some(name) => {
                        edits.push((start, decl_start, String::new()));
                        exports.push(("default".into(), name));
                    }
                    None => {
                        edits.push((start, decl_start, "var __default_export = ".to_string()));
                        exports.push(("default".into(), "__default_export".into()));
                    }
                }
            }
            Statement::ExportAllDeclaration(all) => {
                is_esm = true;
                let spec = all.source.value.to_string();
                specs.push(spec.clone());
                let text = match &all.exported {
                    Some(name) => {
                        let tmp = fresh(&mut counter);
                        exports.push((export_name(name), tmp.clone()));
                        format!("var {} = require({});", tmp, js_str(&spec))
                    }
                    None => format!("__reexport(exports, require({}));", js_str(&spec)),
                };
                edits.push((all.span.start as usize, all.span.end as usize, text));
            }
            Statement::ExportNamedDeclaration(exp) => {
                is_esm = true;
                match &exp.declaration {
                    Some(decl) => {
                        edits.push((
                            exp.span.start as usize,
                            decl.span().start as usize,
                            String::new(),
                        ));
                        match decl {
                            Declaration::VariableDeclaration(v) => {
                                let mut names = Vec::new();
                                for d in &v.declarations {
                                    binding_names(&d.id, &mut names);
                                }
                                for n in names {
                                    exports.push((n.clone(), n));
                                }
                            }
                            Declaration::FunctionDeclaration(f) => {
                                if let Some(id) = &f.id {
                                    exports.push((id.name.to_string(), id.name.to_string()));
                                }
                            }
                            Declaration::ClassDeclaration(c) => {
                                if let Some(id) = &c.id {
                                    exports.push((id.name.to_string(), id.name.to_string()));
                                }
                            }
                            _ => {}
                        }
                    }
                    None => {
                        let mut text = String::new();
                        let tmp = match &exp.source {
                            Some(src) => {
                                let spec = src.value.to_string();
                                specs.push(spec.clone());
                                let tmp = fresh(&mut counter);
                                text = format!("var {} = require({});", tmp, js_str(&spec));
                                Some(tmp)
                            }
                            None => None,
                        };
                        for s in &exp.specifiers {
                            let local = export_name(&s.local);
                            let value = match &tmp {
                                Some(t) => format!("{}[{}]", t, js_str(&local)),
                                None => local,
                            };
                            exports.push((export_name(&s.exported), value));
                        }
                        edits.push((exp.span.start as usize, exp.span.end as usize, text));
                    }
                }
            }
            _ => {}
        }
    }

    // `require(...)` (CommonJS) and `import('...')` anywhere in the module.
    let mut scanner = DepScanner {
        specs: Vec::new(),
        edits: Vec::new(),
    };
    scanner.visit_program(&ret.program);
    specs.extend(scanner.specs);
    // Dynamic-import edits nested inside other edited ranges are dropped.
    for e in scanner.edits {
        if !edits.iter().any(|o| e.0 >= o.0 && e.1 <= o.1) {
            edits.push(e);
        }
    }

    edits.sort_by_key(|e| e.0);
    let mut body = String::with_capacity(code.len() + 256);
    let mut cursor = 0usize;
    for (start, end, text) in edits {
        if start < cursor {
            continue;
        }
        body.push_str(&code[cursor..start]);
        body.push_str(&text);
        cursor = end;
    }
    body.push_str(&code[cursor..]);

    let mut header = String::new();
    if is_esm {
        header.push_str("Object.defineProperty(exports, \"__esModule\", { value: true });\n");
    }
    for (name, value) in &exports {
        header.push_str(&format!(
            "__export(exports, {}, function () {{ return {}; }});\n",
            js_str(name),
            value
        ));
    }
    Ok((format!("{}{}", header, body), specs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_imports_and_exports() {
        let (body, specs) = rewrite_module(
            "import def, { a as b } from './x';\nimport * as ns from './y';\nimport './z';\nexport const c = 1;\nexport function f() {}\nexport { b as bb };\nexport default def;\nexport * from './w';\n",
        )
        .unwrap();
        assert_eq!(specs, vec!["./x", "./y", "./z", "./w"]);
        assert!(body.contains("var def = __default(__m0);"), "{body}");
        assert!(body.contains("var b = __m0[\"a\"];"), "{body}");
        assert!(body.contains("var ns = __m1;"), "{body}");
        assert!(body.contains("__export(exports, \"c\""), "{body}");
        assert!(
            body.contains("__export(exports, \"bb\", function () { return b; })"),
            "{body}"
        );
        assert!(
            body.contains("__reexport(exports, require(\"./w\"))"),
            "{body}"
        );
        assert!(!body.contains("import "), "{body}");
        assert!(body.contains("const c = 1;"), "{body}");
    }

    #[test]
    fn collects_commonjs_requires_and_literal_dynamic_imports() {
        let (body, specs) = rewrite_module(
            "const a = require('./cjs');\nasync function g() { return import('./lazy'); }\n",
        )
        .unwrap();
        assert!(specs.contains(&"./cjs".to_string()));
        assert!(specs.contains(&"./lazy".to_string()));
        assert!(body.contains("Promise.resolve().then"), "{body}");
        assert!(
            !body.contains("__esModule"),
            "commonjs must not be flagged as ESM: {body}"
        );
    }
}
