//! AST-based side-effect detection.
//!
//! Walks the parsed Oxc `Program` to classify top-level statements as
//! side-effect-free (declarations, imports, exports) or side-effectful
//! (calls, assignments, await, IIFEs, global writes).
//!
//! This replaces the string heuristic in `module_source_has_side_effects`
//! which misclassifies strings containing IIFE patterns, comments containing
//! `await`, and can't see inside object literals.

use oxc::allocator::Allocator;
use oxc::ast::ast::*;
use oxc::ast_visit::Visit;
use oxc::parser::{Parser, ParserReturn};
use oxc::span::SourceType;

/// AST-based side-effect detector.
///
/// Returns `true` if the module has top-level side effects — code that
/// executes at module evaluation time and can't be tree-shaken.
///
/// Returns `true` (conservative) if parsing fails — can't tree-shake
/// what we can't parse.
pub fn has_side_effects_ast(source: &str, source_type: SourceType) -> bool {
    let allocator = Allocator::default();
    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, source, source_type).parse();

    if panicked {
        return true;
    }

    let mut detector = SideEffectDetector {
        has_side_effects: false,
    };
    detector.visit_program(&program);
    detector.has_side_effects
}

struct SideEffectDetector {
    has_side_effects: bool,
}

impl SideEffectDetector {
    /// Classify a top-level statement.
    ///
    /// Only top-level statements matter — side effects inside function
    /// bodies don't count (the function might never be called at module
    /// eval time). We do NOT recurse into function bodies.
    fn classify_statement(&mut self, stmt: &Statement) {
        if self.has_side_effects {
            return;
        }

        match stmt {
            // Pure: declarations that don't execute code at eval time
            Statement::ImportDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => {}

            // `export const x = compute();` executes `compute()` at eval
            // time exactly like the non-exported form — the declaration
            // inside must be classified, not waved through.
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(inner) = &decl.declaration {
                    match inner {
                        Declaration::VariableDeclaration(v) => {
                            for d in &v.declarations {
                                if let Some(init) = &d.init {
                                    self.classify_expression(init);
                                }
                            }
                        }
                        Declaration::ClassDeclaration(c) => self.classify_class(c),
                        _ => {}
                    }
                }
            }

            Statement::ExportDefaultDeclaration(decl) => {
                // `export default function Foo() {}` → pure
                // `export default class Foo {}` → pure
                // `export default someExpr` → check the expression
                if let Some(expr) = decl.declaration.as_expression() {
                    self.classify_expression(expr);
                } else if let ExportDefaultDeclarationKind::ClassDeclaration(c) = &decl.declaration
                {
                    self.classify_class(c);
                }
            }

            Statement::VariableDeclaration(decl) => {
                // `const x = 1` → pure
                // `const x = someFunc()` → impure (call at eval time)
                // `const x = await fetch()` → impure (await at eval time)
                for d in &decl.declarations {
                    if let Some(init) = &d.init {
                        self.classify_expression(init);
                    }
                }
            }

            Statement::FunctionDeclaration(_) => {}
            Statement::ClassDeclaration(c) => self.classify_class(c),

            Statement::ExpressionStatement(expr_stmt) => {
                // Top-level expression — could be a call, assignment, etc.
                self.classify_expression(&expr_stmt.expression);
            }

            Statement::DebuggerStatement(_) => {
                self.has_side_effects = true;
            }
            Statement::EmptyStatement(_) => {}
            Statement::BlockStatement(block) => {
                for s in &block.body {
                    self.classify_statement(s);
                }
            }

            // if/for/while/throw/return/break/continue at module scope
            // means code runs at import time — side effect.
            _ => {
                self.has_side_effects = true;
            }
        }
    }

    /// A class definition runs code at eval time through its `extends`
    /// expression and its `static { }` blocks.
    fn classify_class(&mut self, class: &Class) {
        if let Some(sup) = &class.super_class {
            self.classify_expression(sup);
        }
        for el in &class.body.body {
            if matches!(el, ClassElement::StaticBlock(_)) {
                self.has_side_effects = true;
            }
        }
    }

    /// Classify an expression at module top level.
    ///
    /// Only calls, awaits, new, assignments, updates, and yields count
    /// as side effects. Pure reads of identifiers and literals don't.
    fn classify_expression(&mut self, expr: &Expression) {
        if self.has_side_effects {
            return;
        }

        match expr {
            Expression::CallExpression(_)
            | Expression::NewExpression(_)
            | Expression::AwaitExpression(_)
            | Expression::AssignmentExpression(_)
            | Expression::UpdateExpression(_)
            | Expression::YieldExpression(_)
            // `tag`x`` is a call; `import('x')` starts a load.
            | Expression::TaggedTemplateExpression(_)
            | Expression::ImportExpression(_) => {
                self.has_side_effects = true;
            }

            // Wrappers: the effect is in what they wrap.
            Expression::ParenthesizedExpression(p) => self.classify_expression(&p.expression),
            Expression::ChainExpression(_) => {
                // Optional call chains (`a?.()`) — conservatively effectful.
                self.has_side_effects = true;
            }
            Expression::UnaryExpression(u) => {
                if matches!(u.operator, UnaryOperator::Delete) {
                    self.has_side_effects = true;
                } else {
                    self.classify_expression(&u.argument);
                }
            }
            Expression::BinaryExpression(b) => {
                self.classify_expression(&b.left);
                self.classify_expression(&b.right);
            }
            Expression::TSAsExpression(e) => self.classify_expression(&e.expression),
            Expression::TSSatisfiesExpression(e) => self.classify_expression(&e.expression),
            Expression::TSNonNullExpression(e) => self.classify_expression(&e.expression),
            Expression::TSTypeAssertion(e) => self.classify_expression(&e.expression),

            // Recurse into compound expressions — embedded calls count
            Expression::TemplateLiteral(tpl) => {
                for e in &tpl.expressions {
                    self.classify_expression(e);
                }
            }
            Expression::LogicalExpression(logical) => {
                self.classify_expression(&logical.left);
                self.classify_expression(&logical.right);
            }
            Expression::ConditionalExpression(cond) => {
                self.classify_expression(&cond.test);
                self.classify_expression(&cond.consequent);
                self.classify_expression(&cond.alternate);
            }
            Expression::SequenceExpression(seq) => {
                for e in &seq.expressions {
                    self.classify_expression(e);
                }
            }
            Expression::ArrayExpression(arr) => {
                for el in &arr.elements {
                    if let Some(e) = el.as_expression() {
                        self.classify_expression(e);
                    }
                    if let ArrayExpressionElement::SpreadElement(spread) = el {
                        self.classify_expression(&spread.argument);
                    }
                }
            }
            Expression::ObjectExpression(obj) => {
                for prop in &obj.properties {
                    match prop {
                        ObjectPropertyKind::ObjectProperty(p) => {
                            self.classify_expression(&p.value);
                        }
                        ObjectPropertyKind::SpreadProperty(spread) => {
                            self.classify_expression(&spread.argument);
                        }
                    }
                }
            }

            // Pure: identifiers, literals, member access, arrow functions,
            // function expressions, class expressions
            _ => {}
        }
    }
}

impl Visit<'_> for SideEffectDetector {
    fn visit_program(&mut self, program: &Program<'_>) {
        for stmt in &program.body {
            self.classify_statement(stmt);
            if self.has_side_effects {
                return;
            }
        }
    }
}

/// How one `import`/`export … from` statement binds the *target* module.
#[derive(Debug, Clone, Default)]
pub struct ParsedEdge {
    /// The module specifier as written (`"./x"`, `"react"`).
    pub specifier: String,
    /// Named/default imports (`import { a }`, `import d` — `"default"` for
    /// the latter). Bound names conservatively count as used.
    pub import_names: Vec<String>,
    /// Re-exports through this module: `(name imported from the target,
    /// name this module exports it as)`. `export { a as b } from "m"` is
    /// `("a", "b")`; demand flows only if `b` is itself imported downstream.
    pub reexports: Vec<(String, String)>,
    /// `import "x"` — no bindings; keeps the target only for side effects.
    pub side_effect_only: bool,
    /// `import * as ns` / `export *` — opaque to name-level analysis.
    pub all: bool,
}

/// Per-module static analysis shared by the side-effect pass and the
/// export-demand tree shaker: a single Oxc parse producing both.
#[derive(Debug, Clone, Default)]
pub struct ModuleAnalysis {
    /// Top-level side effects (see [`has_side_effects_ast`]).
    pub has_side_effects: bool,
    /// Static `import` / `export … from` statements in source order.
    pub edges: Vec<ParsedEdge>,
}

fn export_name_string(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(n) => n.name.to_string(),
        ModuleExportName::IdentifierReference(n) => n.name.to_string(),
        ModuleExportName::StringLiteral(l) => l.value.to_string(),
    }
}

/// Parse `source` once and classify both its top-level side effects and the
/// shape of every static module edge. `has_side_effects` stays `true` when
/// parsing fails — we never tree-shake what we cannot read.
pub fn analyze_module(source: &str, source_type: SourceType) -> ModuleAnalysis {
    let allocator = Allocator::default();
    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, source, source_type).parse();

    if panicked {
        return ModuleAnalysis {
            has_side_effects: true,
            edges: Vec::new(),
        };
    }

    let mut detector = SideEffectDetector {
        has_side_effects: false,
    };
    detector.visit_program(&program);

    let mut edges: Vec<ParsedEdge> = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(d) => {
                if d.import_kind == ImportOrExportKind::Type {
                    // `import type` is erased at compile time — it must not
                    // keep the target alive (and isn't a real dep edge).
                    continue;
                }
                let mut edge = ParsedEdge {
                    specifier: d.source.value.to_string(),
                    ..Default::default()
                };
                match &d.specifiers {
                    None => edge.side_effect_only = true,
                    Some(specs) if specs.is_empty() => edge.side_effect_only = true,
                    Some(specs) => {
                        for s in specs {
                            match s {
                                ImportDeclarationSpecifier::ImportSpecifier(sp) => {
                                    edge.import_names.push(export_name_string(&sp.imported));
                                }
                                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                                    edge.import_names.push("default".to_string());
                                }
                                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {
                                    edge.all = true;
                                }
                            }
                        }
                    }
                }
                edges.push(edge);
            }
            Statement::ExportAllDeclaration(d) => {
                if d.export_kind == ImportOrExportKind::Type {
                    continue;
                }
                edges.push(ParsedEdge {
                    specifier: d.source.value.to_string(),
                    all: true,
                    ..Default::default()
                });
            }
            Statement::ExportNamedDeclaration(d) => {
                if d.export_kind == ImportOrExportKind::Type || d.source.is_none() {
                    continue;
                }
                let src = d.source.as_ref().unwrap();
                let mut edge = ParsedEdge {
                    specifier: src.value.to_string(),
                    ..Default::default()
                };
                for s in &d.specifiers {
                    edge.reexports.push((
                        export_name_string(&s.local),
                        export_name_string(&s.exported),
                    ));
                }
                edges.push(edge);
            }
            _ => {}
        }
    }

    ModuleAnalysis {
        has_side_effects: detector.has_side_effects,
        edges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_sx(source: &str) -> bool {
        has_side_effects_ast(source, SourceType::mjs())
    }

    #[test]
    fn exported_declarations_with_calls_are_side_effects() {
        assert!(has_sx("export const registered = register();"));
        assert!(has_sx("export let a = 1, b = init();"));
        assert!(!has_sx("export const x = 1; export function f() { g(); }"));
    }

    #[test]
    fn parenthesized_and_wrapped_calls_are_side_effects() {
        assert!(has_sx("const x = (foo());"));
        assert!(has_sx("const x = 1 + foo();"));
        assert!(has_sx("const x = !foo();"));
        assert!(has_sx("const x = tag`a`;"));
        assert!(has_sx("const x = a?.b();"));
        assert!(!has_sx("const x = (1 + 2) * 3;"));
    }

    #[test]
    fn class_extends_call_and_static_blocks_are_side_effects() {
        assert!(has_sx("class A extends mixin(B) {}"));
        assert!(has_sx("export class A extends mixin(B) {}"));
        assert!(has_sx("class A { static { init(); } }"));
        assert!(!has_sx("class A extends B { m() { init(); } }"));
    }

    #[test]
    fn pure_imports() {
        assert!(!has_sx("import { foo } from 'bar';"));
    }

    #[test]
    fn pure_const_literal() {
        assert!(!has_sx("const x = 1;"));
    }

    #[test]
    fn pure_function_decl() {
        assert!(!has_sx("function foo() { console.log('hi'); }"));
    }

    #[test]
    fn pure_class_decl() {
        assert!(!has_sx("class Foo { bar() {} }"));
    }

    #[test]
    fn pure_export_default_function() {
        assert!(!has_sx("export default function Foo() {}"));
    }

    #[test]
    fn pure_export_default_arrow() {
        assert!(!has_sx("export default () => {};"));
    }

    #[test]
    fn impure_top_level_call() {
        assert!(has_sx("console.log('hi');"));
    }

    #[test]
    fn impure_const_with_call() {
        assert!(has_sx("const x = someFunc();"));
    }

    #[test]
    fn impure_top_level_await() {
        assert!(has_sx("await fetch('/api');"));
    }

    #[test]
    fn impure_const_with_await() {
        assert!(has_sx("const x = await fetch('/api');"));
    }

    #[test]
    fn impure_assignment() {
        assert!(has_sx("window.foo = 'bar';"));
    }

    #[test]
    fn impure_update() {
        assert!(has_sx("i++;"));
    }

    #[test]
    fn impure_new_expression() {
        assert!(has_sx("new Foo();"));
    }

    #[test]
    fn impure_iife() {
        assert!(has_sx("(() => { console.log('hi'); })();"));
    }

    #[test]
    fn pure_string_containing_iife_pattern() {
        // The string heuristic falsely flags this; AST walker doesn't.
        assert!(!has_sx("const s = \"(() => foo()\";"));
    }

    #[test]
    fn pure_comment_containing_await() {
        // The string heuristic falsely flags this; AST walker doesn't.
        assert!(!has_sx("// await something\nconst x = 1;"));
    }

    #[test]
    fn impure_export_default_with_call() {
        assert!(has_sx("export default someFunc();"));
    }

    #[test]
    fn impure_export_default_object_with_call() {
        // The string heuristic misses this; AST walker catches it.
        assert!(has_sx("export default { x: foo() };"));
    }

    #[test]
    fn impure_logical_with_call() {
        // The string heuristic misses this; AST walker catches it.
        assert!(has_sx("const x = a && b();"));
    }

    #[test]
    fn impure_debugger() {
        assert!(has_sx("debugger;"));
    }

    #[test]
    fn pure_empty() {
        assert!(!has_sx(""));
    }

    #[test]
    fn pure_multiple_declarations() {
        assert!(!has_sx(
            "const a = 1; const b = 2; function f() {} class C {}"
        ));
    }

    #[test]
    fn impure_if_statement() {
        assert!(has_sx("if (x) { foo(); }"));
    }

    #[test]
    fn impure_throw() {
        assert!(has_sx("throw new Error('oops');"));
    }
}
