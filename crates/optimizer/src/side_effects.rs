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
            | Statement::ExportNamedDeclaration(_)
            | Statement::ExportAllDeclaration(_)
            | Statement::TSTypeAliasDeclaration(_)
            | Statement::TSInterfaceDeclaration(_)
            | Statement::TSEnumDeclaration(_)
            | Statement::TSModuleDeclaration(_)
            | Statement::TSImportEqualsDeclaration(_) => {}

            Statement::ExportDefaultDeclaration(decl) => {
                // `export default function Foo() {}` → pure
                // `export default class Foo {}` → pure
                // `export default someExpr` → check the expression
                if let Some(expr) = decl.declaration.as_expression() {
                    self.classify_expression(expr);
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

            Statement::FunctionDeclaration(_) | Statement::ClassDeclaration(_) => {}

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
            | Expression::YieldExpression(_) => {
                self.has_side_effects = true;
            }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn has_sx(source: &str) -> bool {
        has_side_effects_ast(source, SourceType::mjs())
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
