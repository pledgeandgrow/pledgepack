//! Production bundling: lowers transformed ESM modules into wrapped
//! CommonJS-style module records that execute under the embedded
//! `__pp` runtime (webpack-style, rather than scope hoisting).
//!
//! Pipeline per module:
//!   1. Re-parse the already-transformed code with Oxc — positions in the
//!      generated text are what we splice against, so a second parse is the
//!      only reliable way to get spans for the *output* (not the source).
//!   2. Top-level `import`/`export` declarations become `__pp.req(...)` calls
//!      and live-binding getters on `exports`.
//!   3. Anywhere in the tree: `import()` becomes a chunk-aware `__pp.dyn` /
//!      same-chunk promise, `require("x")` string args are rewritten to module
//!      keys, and `import.meta` becomes a plain object.
//!   4. The lowered body is wrapped in `__pp.def("<key>", fn)`.
//!
//! Resolution is injected as a callback so this module stays pure: the caller
//! (engine emit) maps specifier → module key / external.

use oxc::allocator::Allocator;
use oxc::ast::ast::*;
use oxc::ast_visit::Visit;
use oxc::parser::{Parser, ParserReturn};
use oxc::span::{GetSpan, SourceType, Span};

/// How a specifier was resolved at bundle time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecResolution {
    /// Rewritten to a bundle-internal module key.
    Module(String),
    /// Kept verbatim — external module or unresolvable specifier. The runtime
    /// falls back to a native `require` (Node) or throws.
    External(String),
}

/// Result of lowering one module's transformed code.
pub struct Lowered {
    /// The lowered module body — goes inside the `__pp.def` wrapper.
    pub body: String,
    /// True when any ESM module syntax was lowered (drives `__esModule`).
    pub is_esm: bool,
    /// False when the code could not be parsed and was passed through
    /// unchanged (treated as CommonJS — `require`/`module.exports` still work
    /// inside the wrapper).
    pub lowered: bool,
    /// True when the module body has a top-level `await` (including
    /// `for await (... of ...)`) — a source module with top-level await
    /// (`const x = await import(...)`, `await fetch(...)`, etc.) is valid
    /// ESM, and lowering an `import()` into `__pp.dyn(...)` keeps that
    /// `await` in place. `wrap_module`'s caller must mark the wrapper
    /// `async function` when this is true, or the emitted `await` is a
    /// syntax error (`await` outside an async function) — this must stay
    /// *false* for the common case though: `__pp.req()` is a synchronous
    /// require, and an async wrapper returns a `Promise` from it instead of
    /// `module.exports` directly, breaking every other module's synchronous
    /// `require`.
    pub has_top_level_await: bool,
}

/// Detects whether a module's *top-level* scope contains an `await`
/// expression or a `for await (...)` loop — i.e. whether the module needs an
/// `async` wrapper. Does not descend into nested functions/arrow functions:
/// their own `await` is governed by their own `async` keyword, not the
/// module's.
#[derive(Default)]
struct TopLevelAwaitDetector {
    found: bool,
}

impl<'a> Visit<'a> for TopLevelAwaitDetector {
    fn visit_await_expression(&mut self, it: &AwaitExpression<'a>) {
        self.found = true;
        oxc::ast_visit::walk::walk_await_expression(self, it);
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        if it.r#await {
            self.found = true;
        }
        oxc::ast_visit::walk::walk_for_of_statement(self, it);
    }

    // Function/arrow bodies are their own `async` boundary — stop descent.
    fn visit_function(&mut self, _it: &Function<'a>, _flags: oxc::syntax::scope::ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {}
}

fn has_top_level_await(program: &Program) -> bool {
    let mut detector = TopLevelAwaitDetector::default();
    detector.visit_program(program);
    detector.found
}

/// The `__pp` module runtime, emitted once per chunk file (self-guarding).
///
/// - `m`/`c` — module factory registry and instance cache (global across
///   chunks so async chunks register into the same table).
/// - `chunks` — module key → async chunk URL, filled by `__pp_manifest.js`.
/// - `req` — synchronous require with circular-safe caching.
/// - `d` — default-import interop (`__esModule` aware).
/// - `live` — live-binding export getters.
/// - `star` — `export *` re-export copy.
/// - `dyn` — dynamic import: load chunk if needed, then require.
pub const RUNTIME_PRELUDE: &str = concat!(
    "// pledgepack module runtime\n",
    "!(function(){\n",
    "var P=globalThis.__pp||(globalThis.__pp={m:{},c:{},chunks:{}});\n",
    "P.chunks=P.chunks||{};\n",
    "if(P.def)return;\n",
    "P.def=function(id,f){P.m[id]=f};\n",
    "P.req=function(id){\n",
    "  var c=P.c[id];if(c)return c.exports;\n",
    "  var f=P.m[id];\n",
    "  if(!f){if(typeof require===\"function\")return require(id);\n",
    "    throw new Error(\"[pledgepack] cannot find module: \"+id)}\n",
    "  var m={id:id,exports:{}};P.c[id]=m;\n",
    "  f.call(m.exports,m,m.exports,P.req);\n",
    "  return m.exports\n",
    "};\n",
    "P.d=function(m){return m&&m.__esModule?m.default:m};\n",
    "P.live=function(e){var A=arguments;\n",
    "  for(var i=1;i+1<A.length;i+=2)\n",
    "    Object.defineProperty(e,A[i],{enumerable:true,configurable:true,get:A[i+1]})\n",
    "};\n",
    "P.star=function(e,m){if(!m)return;\n",
    "  for(var k in m)\n",
    "    if(k!==\"default\"&&k!==\"__esModule\"&&!Object.prototype.hasOwnProperty.call(e,k))\n",
    "      (function(k){P.live(e,k,function(){return m[k]})})(k)\n",
    "};\n",
    "P.dyn=function(id){var f=P.chunks[id];\n",
    "  return(f?import(f):Promise.resolve()).then(function(){return P.req(id)})\n",
    "};\n",
    "})();\n",
);

/// Quote a string for embedding in generated JS.
fn js_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One textual edit on the generated code.
#[derive(Debug)]
struct Edit {
    start: u32,
    end: u32,
    text: String,
}

/// Collects expression-level edits anywhere in the tree:
/// `import()`, `import.meta`, and `require("...")` arguments.
struct ExprScanner<'a, R: FnMut(&str) -> SpecResolution> {
    edits: Vec<Edit>,
    /// Keys of dynamic-import targets resolved to bundle modules, in
    /// discovery order — the caller decides same-chunk vs `__pp.dyn`.
    dyn_specs: Vec<(Span, String, SpecResolution)>,
    resolve: &'a mut R,
}

/// Extract the specifier from a static-string expression — covers plain
/// string literals plus the no-substitution template literals the codegen
/// emits (`` import(`./x`) ``).
fn static_spec_expr(expr: &Expression) -> Option<String> {
    match expr {
        Expression::StringLiteral(l) => Some(l.value.to_string()),
        Expression::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            t.quasis[0].value.cooked.as_ref().map(|c| c.to_string())
        }
        _ => None,
    }
}

/// Same as [`static_spec_expr`] for a call `Argument`.
fn static_spec_arg(arg: &Argument) -> Option<String> {
    match arg {
        Argument::StringLiteral(l) => Some(l.value.to_string()),
        Argument::TemplateLiteral(t) if t.expressions.is_empty() && t.quasis.len() == 1 => {
            t.quasis[0].value.cooked.as_ref().map(|c| c.to_string())
        }
        _ => None,
    }
}

impl<'a, R: FnMut(&str) -> SpecResolution> ExprScanner<'a, R> {
    fn resolve_key(&mut self, spec: &str) -> SpecResolution {
        (self.resolve)(spec)
    }
}

impl<'a, R: FnMut(&str) -> SpecResolution> Visit<'a> for ExprScanner<'a, R> {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Some(spec) = static_spec_expr(&it.source) {
            let res = self.resolve_key(&spec);
            self.dyn_specs.push((it.span, spec, res));
            // Replacement decided by the caller — record only.
        }
        oxc::ast_visit::walk::walk_import_expression(self, it);
    }

    fn visit_import_meta(&mut self, it: &ImportMeta) {
        self.edits.push(Edit {
            start: it.span.start,
            end: it.span.end,
            text: "__pp_meta".to_string(),
        });
        oxc::ast_visit::walk::walk_import_meta(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        // require("./x") → require("<module key>") for bundle-internal specs.
        if let Expression::Identifier(callee) = &it.callee
            && callee.name.as_str() == "require"
            && let Some(arg) = it.arguments.first()
            && let Some(spec) = static_spec_arg(arg)
            && let SpecResolution::Module(key) = self.resolve_key(&spec)
            && let Some(span) = arg_span(arg)
        {
            self.edits.push(Edit {
                start: span.start,
                end: span.end,
                text: js_str(&key),
            });
        }
        oxc::ast_visit::walk::walk_call_expression(self, it);
    }
}

/// Span of a string-ish `Argument` (for the `require` argument rewrite).
fn arg_span(arg: &Argument) -> Option<Span> {
    match arg {
        Argument::StringLiteral(l) => Some(l.span),
        Argument::TemplateLiteral(t) => Some(t.span),
        _ => None,
    }
}

/// Extract the bound local names from a binding pattern (`const {a, b: [c]} = x`).
fn binding_names(pattern: &BindingPattern, out: &mut Vec<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPattern::ObjectPattern(p) => {
            for prop in &p.properties {
                binding_names(&prop.value, out);
            }
            if let Some(rest) = &p.rest {
                binding_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(p) => {
            for el in p.elements.iter().flatten() {
                binding_names(el, out);
            }
            if let Some(rest) = &p.rest {
                binding_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(p) => binding_names(&p.left, out),
    }
}

/// Name of a `ModuleExportName` as a property key (`exports.<x>` / `exports["x"]`).
fn export_key(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(n) => n.name.to_string(),
        ModuleExportName::IdentifierReference(n) => n.name.to_string(),
        ModuleExportName::StringLiteral(l) => l.value.to_string(),
    }
}

/// `foo.bar` accessor for a `ModuleExportName` used as a *local* name inside
/// a re-export getter (`__pp_e0.<local>`).
fn local_access(temp: &str, name: &ModuleExportName) -> String {
    let key = export_key(name);
    if !matches!(name, ModuleExportName::StringLiteral(_))
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        format!("{temp}.{key}")
    } else {
        format!("{temp}[{}]", js_str(&key))
    }
}

/// Splice `edits` (absolute positions) into `code`. Edits must not overlap;
/// they are applied right-to-left.
fn apply_edits(code: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = code.to_string();
    for e in edits {
        let (s, t) = (e.start as usize, e.end as usize);
        if s <= t && t <= out.len() && out.is_char_boundary(s) && out.is_char_boundary(t) {
            out.replace_range(s..t, &e.text);
        }
    }
    out
}

/// Extract edits lying inside `span` from `pending`, apply them to the
/// span's slice of `code`, and return the rewritten slice.
fn slice_with_edits(code: &str, span: Span, pending: &mut Vec<Edit>) -> String {
    let (s, e) = (span.start as usize, span.end as usize);
    let slice = code.get(s..e).unwrap_or_default();
    let mut inner: Vec<Edit> = Vec::new();
    pending.retain(|x| {
        if x.start >= span.start && x.end <= span.end {
            inner.push(Edit {
                start: x.start - span.start,
                end: x.end - span.start,
                text: x.text.clone(),
            });
            false
        } else {
            true
        }
    });
    apply_edits(slice, inner)
}

/// Lower transformed `code` into a `__pp`-wrapped module body.
///
/// `resolve` maps each encountered specifier to a module key or external.
/// `dyn_target` decides, per dynamic import, the emitted form — called with
/// the resolved module key (or raw specifier for externals):
///   * `Ok(chunk_file)` — `__pp.dyn("key")` via manifest (caller ensures the
///     manifest maps the key; pass `None` for same-chunk targets)
///
/// Simpler contract: `resolve` returns the module key; the caller separately
/// tells us via `is_same_chunk` whether a dynamic target lives in this chunk.
pub fn lower_module<R, S>(code: &str, mut resolve: R, mut is_same_chunk: S) -> Lowered
where
    R: FnMut(&str) -> SpecResolution,
    S: FnMut(&str, &SpecResolution) -> bool,
{
    let allocator = Allocator::default();
    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if panicked {
        return Lowered {
            body: code.to_string(),
            is_esm: false,
            lowered: false,
            has_top_level_await: false,
        };
    }

    let has_top_level_await = has_top_level_await(&program);

    // Pass 1 — expression-level edits (dynamic import(), import.meta, require()).
    let mut scanner = ExprScanner {
        edits: Vec::new(),
        dyn_specs: Vec::new(),
        resolve: &mut resolve,
    };
    scanner.visit_program(&program);
    let mut expr_edits = std::mem::take(&mut scanner.edits);
    // Take the dynamic-import records out of the scanner so its `&mut resolve`
    // borrow ends — Pass 2 calls `resolve` directly instead of
    // `scanner.resolve_key` (a partially-moved `scanner` can't be borrowed).
    let dyn_specs = std::mem::take(&mut scanner.dyn_specs);

    // Decide dynamic-import replacements now that we know resolutions.
    for (span, spec, res) in dyn_specs {
        let text = match &res {
            SpecResolution::Module(key) => {
                if is_same_chunk(&spec, &res) {
                    format!(
                        "Promise.resolve().then(function(){{return __pp.req({})}})",
                        js_str(key)
                    )
                } else {
                    format!("__pp.dyn({})", js_str(key))
                }
            }
            // External dynamic import: leave the specifier, keep native import().
            SpecResolution::External(raw) => format!("import({})", js_str(raw)),
        };
        expr_edits.push(Edit {
            start: span.start,
            end: span.end,
            text,
        });
    }

    // Pass 2 — top-level module declarations.
    let mut stmt_edits: Vec<Edit> = Vec::new();
    let mut hoisted_imports: Vec<String> = Vec::new();
    let mut export_getters: Vec<(String, String)> = Vec::new(); // (name, getter expr)
    let mut tail_statements: Vec<String> = Vec::new();
    let mut tmp = 0usize;
    let mut is_esm = false;

    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(d) => {
                is_esm = true;
                if d.import_kind == ImportOrExportKind::Type {
                    stmt_edits.push(Edit {
                        start: d.span.start,
                        end: d.span.end,
                        text: String::new(),
                    });
                    continue;
                }
                let key = match resolve(&d.source.value) {
                    SpecResolution::Module(k) | SpecResolution::External(k) => k,
                };
                hoisted_imports.push(import_stmt(
                    d.specifiers.as_ref().map(|v| v.as_slice()),
                    &key,
                    &mut tmp,
                ));
                stmt_edits.push(Edit {
                    start: d.span.start,
                    end: d.span.end,
                    text: String::new(),
                });
            }

            Statement::ExportAllDeclaration(d) => {
                is_esm = true;
                let text = if d.export_kind == ImportOrExportKind::Type {
                    String::new()
                } else {
                    let key = match resolve(&d.source.value) {
                        SpecResolution::Module(k) | SpecResolution::External(k) => k,
                    };
                    match &d.exported {
                        None => format!("__pp.star(exports,__pp.req({}));", js_str(&key)),
                        Some(name) => format!(
                            "exports[{}]=__pp.req({});",
                            js_str(&export_key(name)),
                            js_str(&key)
                        ),
                    }
                };
                stmt_edits.push(Edit {
                    start: d.span.start,
                    end: d.span.end,
                    text,
                });
            }

            Statement::ExportNamedDeclaration(d) => {
                is_esm = true;
                if d.export_kind == ImportOrExportKind::Type {
                    stmt_edits.push(Edit {
                        start: d.span.start,
                        end: d.span.end,
                        text: String::new(),
                    });
                    continue;
                }
                if let Some(src) = &d.source {
                    // export { a as b } from "m"
                    let key = match resolve(&src.value) {
                        SpecResolution::Module(k) | SpecResolution::External(k) => k,
                    };
                    let tmp_name = format!("__pp_e{tmp}");
                    tmp += 1;
                    let mut text = format!("var {tmp_name}=__pp.req({});", js_str(&key));
                    for s in &d.specifiers {
                        let exported = export_key(&s.exported);
                        let access = local_access(&tmp_name, &s.local);
                        text.push_str(&format!(
                            "__pp.live(exports,{},function(){{return {access}}});",
                            js_str(&exported)
                        ));
                    }
                    stmt_edits.push(Edit {
                        start: d.span.start,
                        end: d.span.end,
                        text,
                    });
                } else if let Some(decl) = &d.declaration {
                    // export <decl> — keep the declaration, install getters at end.
                    let inner = slice_with_edits(code, decl.span(), &mut expr_edits);
                    let mut names = Vec::new();
                    match decl {
                        Declaration::VariableDeclaration(v) => {
                            for d in &v.declarations {
                                binding_names(&d.id, &mut names);
                            }
                        }
                        Declaration::FunctionDeclaration(f) => {
                            if let Some(id) = &f.id {
                                names.push(id.name.to_string());
                            }
                        }
                        Declaration::ClassDeclaration(c) => {
                            if let Some(id) = &c.id {
                                names.push(id.name.to_string());
                            }
                        }
                        _ => {}
                    }
                    for n in names {
                        export_getters.push((n.clone(), n));
                    }
                    stmt_edits.push(Edit {
                        start: d.span.start,
                        end: d.span.end,
                        text: inner,
                    });
                } else {
                    // export { a, b as c } — no source, no declaration.
                    for s in &d.specifiers {
                        let exported = export_key(&s.exported);
                        let local = match &s.local {
                            ModuleExportName::IdentifierName(n) => n.name.to_string(),
                            ModuleExportName::IdentifierReference(n) => n.name.to_string(),
                            ModuleExportName::StringLiteral(l) => l.value.to_string(),
                        };
                        export_getters.push((exported, local));
                    }
                    stmt_edits.push(Edit {
                        start: d.span.start,
                        end: d.span.end,
                        text: String::new(),
                    });
                }
            }

            Statement::ExportDefaultDeclaration(d) => {
                is_esm = true;
                let span = d.span;
                let text = match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        let inner = slice_with_edits(code, f.span, &mut expr_edits);
                        match &f.id {
                            Some(id) => {
                                let name = id.name.to_string();
                                export_getters.push(("default".to_string(), name));
                                inner
                            }
                            None => format!("exports.default={inner}"),
                        }
                    }
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => {
                        let inner = slice_with_edits(code, c.span, &mut expr_edits);
                        match &c.id {
                            Some(id) => {
                                let name = id.name.to_string();
                                export_getters.push(("default".to_string(), name));
                                inner
                            }
                            None => format!("exports.default={inner}"),
                        }
                    }
                    ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => String::new(),
                    other => {
                        let inner = slice_with_edits(code, other.span(), &mut expr_edits);
                        format!("exports.default={inner};")
                    }
                };
                stmt_edits.push(Edit {
                    start: span.start,
                    end: span.end,
                    text,
                });
            }

            _ => {}
        }
    }

    // Collapse export getters into __pp.live calls appended at module end.
    if !export_getters.is_empty() {
        let mut s = String::from("__pp.live(exports");
        for (name, local) in export_getters {
            s.push_str(&format!(",{},function(){{return {local}}}", js_str(&name)));
        }
        s.push_str(");");
        tail_statements.push(s);
    }

    // Insertion point for hoisted imports: after the directive prologue.
    let insert_pos = program.directives.last().map(|d| d.span.end).unwrap_or(0);
    let mut prologue = String::new();
    prologue.push_str("\nvar __pp_meta={url:\"\",env:{MODE:\"production\",DEV:!1,PROD:!0,SSR:!1,BASE_URL:\"/\"}};\n");
    if is_esm {
        prologue.push_str("Object.defineProperty(exports,\"__esModule\",{value:!0});\n");
    }
    for imp in &hoisted_imports {
        prologue.push_str(imp);
        prologue.push('\n');
    }
    stmt_edits.push(Edit {
        start: insert_pos,
        end: insert_pos,
        text: prologue,
    });
    let tail = tail_statements.concat();
    if !tail.is_empty() {
        stmt_edits.push(Edit {
            start: code.len() as u32,
            end: code.len() as u32,
            text: format!("\n{tail}"),
        });
    }

    expr_edits.extend(stmt_edits);
    Lowered {
        body: apply_edits(code, expr_edits),
        is_esm,
        lowered: true,
        has_top_level_await,
    }
}

/// Build the `__pp.req(...)` import statements for an `ImportDeclaration`.
fn import_stmt(specs: Option<&[ImportDeclarationSpecifier]>, key: &str, tmp: &mut usize) -> String {
    let req = format!("__pp.req({})", js_str(key));
    let specs = match specs {
        None => return format!("{req};"),
        Some([]) => return format!("{req};"),
        Some(s) => s,
    };

    let mut default: Option<String> = None;
    let mut namespace: Option<String> = None;
    let mut named: Vec<(String, String, bool)> = Vec::new(); // (imported, local, imported_is_str)

    for s in specs {
        match s {
            ImportDeclarationSpecifier::ImportDefaultSpecifier(d) => {
                default = Some(d.local.name.to_string());
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(n) => {
                namespace = Some(n.local.name.to_string());
            }
            ImportDeclarationSpecifier::ImportSpecifier(i) => {
                let (imported, is_str) = match &i.imported {
                    ModuleExportName::IdentifierName(n) => (n.name.to_string(), false),
                    ModuleExportName::IdentifierReference(n) => (n.name.to_string(), false),
                    ModuleExportName::StringLiteral(l) => (l.value.to_string(), true),
                };
                named.push((imported, i.local.name.to_string(), is_str));
            }
        }
    }

    // Does any named specifier import the `default` binding?
    let has_default_named = named.iter().any(|(i, _, _)| i == "default");
    let needs_tmp = (default.is_some() || namespace.is_some() || has_default_named)
        && !named.is_empty()
        || (default.is_some() && namespace.is_some());

    let mut out = String::new();
    let req_target = if needs_tmp {
        let t = format!("__pp_i{tmp}");
        *tmp += 1;
        out.push_str(&format!("var {t}={req};"));
        t
    } else {
        String::new()
    };
    let base = if needs_tmp {
        req_target.clone()
    } else {
        req.clone()
    };

    if let Some(d) = default {
        out.push_str(&format!("const {d}=__pp.d({base});"));
    }
    if let Some(n) = namespace {
        out.push_str(&format!("const {n}={base};"));
    }
    // Remaining named specifiers.
    let rest: Vec<&(String, String, bool)> =
        named.iter().filter(|(i, _, _)| i != "default").collect();
    if has_default_named {
        // `import { default as d }` → the default export.
        for (_, local, _) in named.iter().filter(|(i, _, _)| i == "default") {
            out.push_str(&format!("const {local}=__pp.d({base});"));
        }
    }
    if !rest.is_empty() {
        let mut destructure = String::from("const{");
        for (i, (imported, local, is_str)) in rest.iter().enumerate() {
            if i > 0 {
                destructure.push(',');
            }
            if *is_str {
                destructure.push_str(&format!("{}:{}", js_str(imported), local));
            } else if imported == local {
                destructure.push_str(local);
            } else {
                destructure.push_str(&format!("{imported}:{local}"));
            }
        }
        destructure.push_str(&format!("}}={base};"));
        out.push_str(&destructure);
    }
    out
}

/// Wrap a lowered module body in its `__pp.def` registration.
///
/// `is_async` must be true when `body` has a top-level `await`
/// ([`Lowered::has_top_level_await`]) — the factory is invoked synchronously
/// by `P.req` (`f.call(m.exports,m,m.exports,P.req)`, return value discarded)
/// regardless of whether it's `async`, so marking it `async` here is safe for
/// every module: modules without top-level await behave exactly as before,
/// and modules with one get a function `await` is actually legal in, instead
/// of `SyntaxError: Unexpected reserved word` crashing the whole bundle.
pub fn wrap_module(key: &str, body: &str, is_async: bool) -> String {
    format!(
        "__pp.def({},{}function(module,exports,require){{\n{}\n}});\n",
        js_str(key),
        if is_async { "async " } else { "" },
        body
    )
}

/// Public string-quoting helper for emit code that needs to embed a JS
/// string literal.
pub fn js_str_pub(s: &str) -> String {
    js_str(s)
}

/// The manifest file: maps module keys to async chunk files for `__pp.dyn`.
pub fn manifest_code(map: &[(String, String)]) -> String {
    let mut s = String::from(
        "// pledgepack async-chunk manifest\nglobalThis.__pp=globalThis.__pp||{m:{},c:{},chunks:{}};\n__pp.chunks=__pp.chunks||{};\n",
    );
    for (k, v) in map {
        s.push_str(&format!("__pp.chunks[{}]={};\n", js_str(k), js_str(v)));
    }
    s
}

/// Parse emitted chunk JS and return the parser's error messages.
/// Used by `build --verify`: emitted output must be syntactically valid.
pub fn chunk_syntax_errors(code: &str, filename: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, code, SourceType::mjs()).parse();
    ret.diagnostics
        .iter()
        .map(|e| format!("{filename}: {e}"))
        .collect()
}

/// Scan already-emitted bundle code for specifiers that were *not* resolved —
/// used by `build --verify`. Returns every `from "x"` / `import("x")` /
/// `require("x")` string specifier still present.
pub fn scan_unresolved_specifiers(code: &str) -> Vec<String> {
    let allocator = Allocator::default();
    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, code, SourceType::mjs()).parse();
    if panicked {
        return Vec::new();
    }
    struct V {
        specs: Vec<String>,
    }
    impl<'a> Visit<'a> for V {
        fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
            if let Some(spec) = static_spec_expr(&it.source) {
                self.specs.push(spec);
            }
            oxc::ast_visit::walk::walk_import_expression(self, it);
        }
        fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
            self.specs.push(it.source.value.to_string());
            oxc::ast_visit::walk::walk_import_declaration(self, it);
        }
        fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
            if let Expression::Identifier(c) = &it.callee
                && c.name.as_str() == "require"
                && let Some(arg) = it.arguments.first()
                && let Some(spec) = static_spec_arg(arg)
            {
                self.specs.push(spec);
            }
            oxc::ast_visit::walk::walk_call_expression(self, it);
        }
        fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
            self.specs.push(it.source.value.to_string());
            oxc::ast_visit::walk::walk_export_all_declaration(self, it);
        }
        fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
            if let Some(s) = &it.source {
                self.specs.push(s.value.to_string());
            }
            oxc::ast_visit::walk::walk_export_named_declaration(self, it);
        }
    }
    let mut v = V { specs: Vec::new() };
    v.visit_program(&program);
    v.specs
}

/// Returns `true` when `offset` sits inside a string literal or comment in
/// `source`. Used to rescue the raw dep scanners: `import("x")` /
/// `require("x")` matches inside prose or error-message strings (e.g.
/// React's `lazy()` hint text) are not real dependencies.
///
/// Handles `'…'`, `"…"`, template literals with `${}` interpolation (nested
/// templates included), `//` and `/* */` comments, and `\` escapes. Regex
/// literals are not distinguished from division — a `/` containing quote or
/// comment bytes can skew the state — so callers should only use this to
/// *rescue* a specifier that already failed to resolve, never to drop one
/// that resolved fine.
pub(crate) fn in_string_or_comment(source: &str, offset: usize) -> bool {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        Code,
        Single,
        Double,
        Template,
        LineComment,
        BlockComment,
    }

    let bytes = source.as_bytes();
    let end = offset.min(bytes.len());
    let mut st = St::Code;
    // Brace depth of the innermost `${ }` interpolations, innermost last.
    let mut interp: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < end {
        let b = bytes[i];
        let next = bytes.get(i + 1).copied();
        match st {
            St::Code => match b {
                b'\'' => st = St::Single,
                b'"' => st = St::Double,
                b'`' => st = St::Template,
                b'/' if next == Some(b'/') => {
                    st = St::LineComment;
                    i += 1;
                }
                b'/' if next == Some(b'*') => {
                    st = St::BlockComment;
                    i += 1;
                }
                b'{' if !interp.is_empty() => *interp.last_mut().unwrap() += 1,
                b'}' if !interp.is_empty() => {
                    let d = interp.last_mut().unwrap();
                    *d -= 1;
                    if *d == 0 {
                        interp.pop();
                        st = St::Template;
                    }
                }
                _ => {}
            },
            St::Single => {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == b'\'' || b == b'\n' {
                    st = St::Code;
                }
            }
            St::Double => {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == b'"' || b == b'\n' {
                    st = St::Code;
                }
            }
            St::Template => {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == b'`' {
                    st = St::Code;
                } else if b == b'$' && next == Some(b'{') {
                    interp.push(1);
                    st = St::Code;
                    i += 1;
                }
            }
            St::LineComment => {
                if b == b'\n' {
                    st = St::Code;
                }
            }
            St::BlockComment => {
                if b == b'*' && next == Some(b'/') {
                    st = St::Code;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    st != St::Code
}

/// Returns `true` when every quoted occurrence of `spec` in `source` sits
/// inside a string literal or comment — i.e. the raw scanners matched
/// `import('spec')`/`require('spec')` text inside prose, not real code.
/// Returns `false` when no quoted occurrence exists at all, so genuine
/// unresolved imports still fail the build.
pub(crate) fn specifier_only_in_strings(source: &str, spec: &str) -> bool {
    let mut found = false;
    for q in ['"', '\''] {
        let needle = format!("{q}{spec}{q}");
        let mut pos = 0;
        while let Some(rel) = source[pos..].find(&needle) {
            let abs = pos + rel;
            // The opening quote is inside another literal only when the
            // match is prose, not a real specifier.
            if !in_string_or_comment(source, abs) {
                return false;
            }
            found = true;
            pos = abs + needle.len();
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specifier_in_error_message_string_is_ignored() {
        // React's lazy() hint text contains literal `import('./MyComponent')`.
        let src = r#"const msg = "Use lazy(() => import('./MyComponent'))";"#;
        assert!(specifier_only_in_strings(src, "./MyComponent"));
    }

    #[test]
    fn specifier_in_line_comment_is_ignored() {
        let src = "// usage: require('./polyfill')\nconst x = 1;";
        assert!(specifier_only_in_strings(src, "./polyfill"));
    }

    #[test]
    fn specifier_in_block_comment_is_ignored() {
        let src = "/* require('./polyfill') */\nconst x = 1;";
        assert!(specifier_only_in_strings(src, "./polyfill"));
    }

    #[test]
    fn specifier_in_template_literal_is_ignored() {
        let src = "const tpl = `see import('./inner') for details`;";
        assert!(specifier_only_in_strings(src, "./inner"));
    }

    #[test]
    fn real_import_is_not_ignored() {
        let src = "import { x } from './real';\nexport { x };";
        assert!(!specifier_only_in_strings(src, "./real"));
    }

    #[test]
    fn real_require_is_not_ignored() {
        let src = "const dep = require('./dep');";
        assert!(!specifier_only_in_strings(src, "./dep"));
    }

    #[test]
    fn real_dynamic_import_is_not_ignored() {
        let src = "const lazy = () => import('./lazy');";
        assert!(!specifier_only_in_strings(src, "./lazy"));
    }

    #[test]
    fn mixed_string_and_code_occurrences_are_not_ignored() {
        // Same specifier in prose AND real code → the real occurrence must
        // keep resolution failures fatal.
        let src = "const doc = \"import('./dup')\";\nimport('./dup');";
        assert!(!specifier_only_in_strings(src, "./dup"));
    }

    #[test]
    fn absent_specifier_is_not_ignored() {
        let src = "const x = 1;";
        assert!(!specifier_only_in_strings(src, "./nope"));
    }

    fn lower(src: &str) -> Lowered {
        lower_module(
            src,
            |spec| SpecResolution::Module(spec.trim_start_matches("./").to_string()),
            |_, _| true,
        )
    }

    #[test]
    fn top_level_await_of_dynamic_import_is_detected_and_wrapped_async() {
        // The exact real-world shape this bug came from: a source module
        // awaiting a lowered `import()` at its top level.
        let lowered = lower("const { lazy } = await import('./lazy.ts');\nlazy();");
        assert!(
            lowered.has_top_level_await,
            "await at module top level must be detected"
        );
        let wrapped = wrap_module("entry", &lowered.body, lowered.has_top_level_await);
        assert!(
            wrapped.contains("async function"),
            "wrapper must be `async function` when the body has top-level await: {wrapped}"
        );
        // The lowered await must still be there — this is what previously
        // crashed with `SyntaxError: Unexpected reserved word` because the
        // wrapper stayed a plain `function`.
        assert!(wrapped.contains("await"));
    }

    #[test]
    fn for_await_of_at_top_level_is_detected() {
        let lowered = lower("for await (const x of gen()) { use(x); }");
        assert!(lowered.has_top_level_await);
    }

    #[test]
    fn plain_module_without_await_stays_a_sync_wrapper() {
        let lowered = lower("export const x = 1;\nconsole.log(x);");
        assert!(!lowered.has_top_level_await);
        let wrapped = wrap_module("m", &lowered.body, lowered.has_top_level_await);
        assert!(
            !wrapped.contains("async function"),
            "must not gain `async` when there's no top-level await: {wrapped}"
        );
    }

    #[test]
    fn await_inside_a_nested_async_function_is_not_top_level_await() {
        // The nested function's own `async` covers its own `await` — the
        // *module* itself never suspends, so its wrapper must stay sync
        // (this is also what keeps `__pp.req()`'s synchronous-require
        // contract working for every module that merely *contains* async
        // code without awaiting anything at its own top level).
        let lowered = lower("async function f() { await g(); }\nexport { f };");
        assert!(
            !lowered.has_top_level_await,
            "await inside a nested async function must not count as top-level"
        );
    }

    #[test]
    fn await_inside_a_nested_arrow_function_is_not_top_level_await() {
        let lowered = lower("const f = async () => { await g(); };\nexport { f };");
        assert!(!lowered.has_top_level_await);
    }

    #[test]
    fn await_inside_an_if_block_at_top_level_is_still_top_level() {
        // Not inside any function — nesting in ordinary control flow doesn't
        // create a new async boundary the way a function body does.
        let lowered = lower("if (cond) { await ready(); }");
        assert!(lowered.has_top_level_await);
    }
}
