//! Oxc AST → ESTree JSON converter.
//!
//! ESTree is the de facto standard AST format used by Babel, ESLint,
//! PostCSS, and most JS tooling. WASM plugins receive this format via
//! the WIT contract's `get_plugin_ast` host import.
//!
//! This is a hand-written converter rather than `oxc/serde` because:
//! 1. Oxc's AST types don't 1:1 match ESTree (different field names,
//!    different enum variants, Oxc-specific nodes).
//! 2. We control the output format exactly, matching what Babel/ESLint expect.
//! 3. No additional feature flag or compile-time dependency on oxc/serde.

use serde_json::{json, Value};
use oxc::ast::ast::*;
use oxc::ast_visit::Visit;

/// Convert an Oxc `Program` to ESTree-compatible JSON.
pub fn program_to_estree(program: &Program<'_>) -> Value {
    let mut converter = EstreeConverter::new();
    converter.visit_program(program);
    eprintln!("converter.body.len() = {}", converter.body.len());
    json!({
        "type": "Program",
        "sourceType": if program.source_type.is_module() { "module" } else { "script" },
        "body": converter.body,
    })
}

struct EstreeConverter {
    body: Vec<Value>,
}

impl EstreeConverter {
    fn new() -> Self {
        Self { body: Vec::new() }
    }

    fn convert_statement(&mut self, stmt: &Statement) -> Value {
        match stmt {
            Statement::ImportDeclaration(decl) => {
                let specifiers = decl.specifiers.as_ref().map(|specs| {
                    specs.iter().map(|s| self.convert_import_specifier(s)).collect::<Vec<_>>()
                }).unwrap_or_default();
                json!({
                    "type": "ImportDeclaration",
                    "specifiers": specifiers,
                    "source": self.convert_string_literal(&decl.source),
                })
            }
            Statement::ExportNamedDeclaration(decl) => {
                json!({
                    "type": "ExportNamedDeclaration",
                    "declaration": decl.declaration.as_ref().map(|d| self.convert_declaration(d)),
                    "specifiers": decl.specifiers.iter().map(|s| self.convert_export_specifier(s)).collect::<Vec<_>>(),
                    "source": decl.source.as_ref().map(|s| self.convert_string_literal(s)),
                })
            }
            Statement::ExportDefaultDeclaration(decl) => {
                let declaration = if let Some(expr) = decl.declaration.as_expression() {
                    self.convert_expression(expr)
                } else {
                    json!({"type": "UnknownExportDefault"})
                };
                json!({
                    "type": "ExportDefaultDeclaration",
                    "declaration": declaration,
                })
            }
            Statement::ExportAllDeclaration(decl) => {
                json!({
                    "type": "ExportAllDeclaration",
                    "source": self.convert_string_literal(&decl.source),
                })
            }
            Statement::VariableDeclaration(decl) => {
                json!({
                    "type": "VariableDeclaration",
                    "kind": match decl.kind {
                        VariableDeclarationKind::Var => "var",
                        VariableDeclarationKind::Let => "let",
                        VariableDeclarationKind::Const => "const",
                        _ => "var",
                    },
                    "declarations": decl.declarations.iter().map(|d| self.convert_var_declarator(d)).collect::<Vec<_>>(),
                })
            }
            Statement::FunctionDeclaration(decl) => {
                self.convert_function(
                    "FunctionDeclaration",
                    decl.id.as_ref(),
                    &decl.params,
                    decl.body.as_deref(),
                    decl.r#async,
                    decl.generator,
                )
            }
            Statement::ClassDeclaration(decl) => {
                self.convert_class("ClassDeclaration", decl.id.as_ref(), &decl.body)
            }
            Statement::ExpressionStatement(stmt) => {
                json!({
                    "type": "ExpressionStatement",
                    "expression": self.convert_expression(&stmt.expression),
                })
            }
            Statement::EmptyStatement(_) => json!({"type": "EmptyStatement"}),
            Statement::BlockStatement(block) => {
                let mut body = Vec::new();
                for s in &block.body {
                    body.push(self.convert_statement(s));
                }
                json!({"type": "BlockStatement", "body": body})
            }
            Statement::DebuggerStatement(_) => json!({"type": "DebuggerStatement"}),
            // TypeScript declarations
            Statement::TSTypeAliasDeclaration(decl) => {
                json!({"type": "TSTypeAliasDeclaration", "id": self.convert_binding_identifier(&decl.id)})
            }
            Statement::TSInterfaceDeclaration(decl) => {
                json!({"type": "TSInterfaceDeclaration", "id": self.convert_binding_identifier(&decl.id)})
            }
            Statement::TSEnumDeclaration(decl) => {
                json!({"type": "TSEnumDeclaration", "id": self.convert_binding_identifier(&decl.id)})
            }
            _ => json!({"type": "UnknownStatement"}),
        }
    }

    fn convert_expression(&mut self, expr: &Expression) -> Value {
        match expr {
            Expression::Identifier(ident) => {
                json!({"type": "Identifier", "name": ident.name.as_str()})
            }
            Expression::StringLiteral(lit) => {
                json!({"type": "StringLiteral", "value": lit.value.as_str()})
            }
            Expression::NumericLiteral(lit) => {
                json!({"type": "NumericLiteral", "value": lit.value})
            }
            Expression::BooleanLiteral(lit) => {
                json!({"type": "BooleanLiteral", "value": lit.value})
            }
            Expression::NullLiteral(_) => {
                json!({"type": "NullLiteral"})
            }
            Expression::TemplateLiteral(lit) => {
                json!({
                    "type": "TemplateLiteral",
                    "quasis": lit.quasis.iter().map(|q| {
                        json!({
                            "type": "TemplateElement",
                            "value": {
                                "raw": q.value.raw.as_str(),
                                "cooked": q.value.cooked.as_ref().map(|c| c.as_str()),
                            },
                            "tail": q.tail,
                        })
                    }).collect::<Vec<_>>(),
                    "expressions": lit.expressions.iter().map(|e| self.convert_expression(e)).collect::<Vec<_>>(),
                })
            }
            Expression::CallExpression(call) => {
                json!({
                    "type": "CallExpression",
                    "callee": self.convert_expression(&call.callee),
                    "arguments": call.arguments.iter().map(|a| self.convert_argument(a)).collect::<Vec<_>>(),
                })
            }
            Expression::NewExpression(new_expr) => {
                json!({
                    "type": "NewExpression",
                    "callee": self.convert_expression(&new_expr.callee),
                    "arguments": new_expr.arguments.iter().map(|a| self.convert_argument(a)).collect::<Vec<_>>(),
                })
            }
            Expression::StaticMemberExpression(member) => {
                json!({
                    "type": "MemberExpression",
                    "object": self.convert_expression(&member.object),
                    "property": json!({"type": "Identifier", "name": member.property.name.as_str()}),
                    "computed": false,
                })
            }
            Expression::ComputedMemberExpression(member) => {
                json!({
                    "type": "MemberExpression",
                    "object": self.convert_expression(&member.object),
                    "property": self.convert_expression(&member.expression),
                    "computed": true,
                })
            }
            Expression::AssignmentExpression(assign) => {
                json!({
                    "type": "AssignmentExpression",
                    "operator": format!("{:?}", assign.operator),
                    "left": self.convert_assignment_target(&assign.left),
                    "right": self.convert_expression(&assign.right),
                })
            }
            Expression::ArrowFunctionExpression(arrow) => {
                let body = if arrow.expression {
                    if let Some(body_stmt) = arrow.body.statements.first() {
                        if let Statement::ExpressionStatement(expr_stmt) = body_stmt {
                            json!({
                                "type": "BlockStatement",
                                "body": [{"type": "ReturnStatement", "argument": self.convert_expression(&expr_stmt.expression)}]
                            })
                        } else {
                            json!({"type": "BlockStatement", "body": []})
                        }
                    } else {
                        json!({"type": "BlockStatement", "body": []})
                    }
                } else {
                    let mut stmts = Vec::new();
                    for s in &arrow.body.statements {
                        stmts.push(self.convert_statement(s));
                    }
                    json!({"type": "BlockStatement", "body": stmts})
                };
                json!({
                    "type": "ArrowFunctionExpression",
                    "params": arrow.params.items.iter().map(|p| self.convert_formal_parameter(p)).collect::<Vec<_>>(),
                    "body": body,
                    "async": arrow.r#async,
                    "expression": arrow.expression,
                })
            }
            Expression::ObjectExpression(obj) => {
                json!({
                    "type": "ObjectExpression",
                    "properties": obj.properties.iter().map(|p| self.convert_object_property(p)).collect::<Vec<_>>(),
                })
            }
            Expression::ArrayExpression(arr) => {
                json!({
                    "type": "ArrayExpression",
                    "elements": arr.elements.iter().map(|el| {
                        if let Some(e) = el.as_expression() {
                            self.convert_expression(e)
                        } else if let ArrayExpressionElement::SpreadElement(spread) = el {
                            json!({"type": "SpreadElement", "argument": self.convert_expression(&spread.argument)})
                        } else {
                            Value::Null
                        }
                    }).collect::<Vec<_>>(),
                })
            }
            Expression::BinaryExpression(bin) => {
                json!({
                    "type": "BinaryExpression",
                    "operator": format!("{:?}", bin.operator),
                    "left": self.convert_expression(&bin.left),
                    "right": self.convert_expression(&bin.right),
                })
            }
            Expression::LogicalExpression(logical) => {
                json!({
                    "type": "LogicalExpression",
                    "operator": format!("{:?}", logical.operator),
                    "left": self.convert_expression(&logical.left),
                    "right": self.convert_expression(&logical.right),
                })
            }
            Expression::ConditionalExpression(cond) => {
                json!({
                    "type": "ConditionalExpression",
                    "test": self.convert_expression(&cond.test),
                    "consequent": self.convert_expression(&cond.consequent),
                    "alternate": self.convert_expression(&cond.alternate),
                })
            }
            Expression::AwaitExpression(await_expr) => {
                json!({
                    "type": "AwaitExpression",
                    "argument": self.convert_expression(&await_expr.argument),
                })
            }
            Expression::UpdateExpression(update) => {
                json!({
                    "type": "UpdateExpression",
                    "operator": format!("{:?}", update.operator),
                    "argument": self.convert_simple_assignment_target(&update.argument),
                    "prefix": update.prefix,
                })
            }
            Expression::YieldExpression(yield_expr) => {
                json!({
                    "type": "YieldExpression",
                    "argument": yield_expr.argument.as_ref().map(|a| self.convert_expression(a)),
                })
            }
            Expression::ImportExpression(import) => {
                json!({
                    "type": "ImportExpression",
                    "source": self.convert_expression(&import.source),
                })
            }
            Expression::FunctionExpression(func) => {
                self.convert_function(
                    "FunctionExpression",
                    func.id.as_ref(),
                    &func.params,
                    func.body.as_deref(),
                    func.r#async,
                    func.generator,
                )
            }
            Expression::ClassExpression(cls) => {
                self.convert_class("ClassExpression", cls.id.as_ref(), &cls.body)
            }
            Expression::ThisExpression(_) => {
                json!({"type": "ThisExpression"})
            }
            Expression::ParenthesizedExpression(paren) => {
                self.convert_expression(&paren.expression)
            }
            _ => json!({"type": "UnknownExpression"}),
        }
    }

    // ─── Helper methods ──────────────────────────────────────────────

    fn convert_string_literal(&self, lit: &StringLiteral<'_>) -> Value {
        json!({"type": "StringLiteral", "value": lit.value.as_str()})
    }

    fn convert_binding_identifier(&self, ident: &BindingIdentifier<'_>) -> Value {
        json!({"type": "Identifier", "name": ident.name.as_str()})
    }

    fn convert_import_specifier(&self, spec: &ImportDeclarationSpecifier<'_>) -> Value {
        match spec {
            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                json!({
                    "type": "ImportSpecifier",
                    "imported": json!({"type": "Identifier", "name": s.imported.name().as_str()}),
                    "local": json!({"type": "Identifier", "name": s.local.name.as_str()}),
                })
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                json!({
                    "type": "ImportDefaultSpecifier",
                    "local": json!({"type": "Identifier", "name": s.local.name.as_str()}),
                })
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                json!({
                    "type": "ImportNamespaceSpecifier",
                    "local": json!({"type": "Identifier", "name": s.local.name.as_str()}),
                })
            }
        }
    }

    fn convert_export_specifier(&self, spec: &ExportSpecifier<'_>) -> Value {
        // ExportSpecifier is a struct with exported/local fields
        json!({
            "type": "ExportSpecifier",
            "exported": json!({"type": "Identifier", "name": spec.exported.name().as_str()}),
            "local": json!({"type": "Identifier", "name": spec.local.name().as_str()}),
        })
    }

    fn convert_var_declarator(&mut self, decl: &VariableDeclarator<'_>) -> Value {
        json!({
            "type": "VariableDeclarator",
            "id": self.convert_binding_pattern(&decl.id),
            "init": decl.init.as_ref().map(|e| self.convert_expression(e)),
        })
    }

    fn convert_binding_pattern(&self, pattern: &BindingPattern<'_>) -> Value {
        match pattern {
            BindingPattern::BindingIdentifier(ident) => {
                json!({"type": "Identifier", "name": ident.name.as_str()})
            }
            BindingPattern::ArrayPattern(arr) => {
                json!({"type": "ArrayPattern", "elements": arr.elements.len()})
            }
            BindingPattern::ObjectPattern(obj) => {
                json!({"type": "ObjectPattern", "properties": obj.properties.len()})
            }
            _ => json!({"type": "UnknownPattern"}),
        }
    }

    fn convert_formal_parameter(&self, param: &FormalParameter<'_>) -> Value {
        self.convert_binding_pattern(&param.pattern)
    }

    fn convert_object_property(&mut self, prop: &ObjectPropertyKind<'_>) -> Value {
        match prop {
            ObjectPropertyKind::ObjectProperty(p) => {
                let key = match &p.key {
                    PropertyKey::Identifier(ident) => {
                        json!({"type": "Identifier", "name": ident.name.as_str()})
                    }
                    PropertyKey::PrivateIdentifier(ident) => {
                        json!({"type": "PrivateIdentifier", "name": ident.name.as_str()})
                    }
                    _ => {
                        if let Some(expr) = p.key.as_expression() {
                            self.convert_expression(expr)
                        } else {
                            Value::Null
                        }
                    }
                };
                json!({
                    "type": "ObjectProperty",
                    "key": key,
                    "value": self.convert_expression(&p.value),
                    "computed": p.computed,
                    "shorthand": p.shorthand,
                })
            }
            ObjectPropertyKind::SpreadProperty(spread) => {
                json!({
                    "type": "SpreadElement",
                    "argument": self.convert_expression(&spread.argument),
                })
            }
        }
    }

    fn convert_argument(&mut self, arg: &Argument<'_>) -> Value {
        if let Some(expr) = arg.as_expression() {
            self.convert_expression(expr)
        } else {
            // SpreadElement in arguments
            json!({"type": "SpreadElement"})
        }
    }

    fn convert_assignment_target(&mut self, target: &AssignmentTarget<'_>) -> Value {
        if let Some(expr) = target.get_expression() {
            self.convert_expression(expr)
        } else {
            json!({"type": "UnknownAssignmentTarget"})
        }
    }

    fn convert_simple_assignment_target(&mut self, target: &SimpleAssignmentTarget<'_>) -> Value {
        if let Some(expr) = target.get_expression() {
            self.convert_expression(expr)
        } else {
            json!({"type": "UnknownAssignmentTarget"})
        }
    }

    fn convert_declaration(&mut self, decl: &Declaration<'_>) -> Value {
        match decl {
            Declaration::VariableDeclaration(d) => {
                json!({
                    "type": "VariableDeclaration",
                    "kind": match d.kind {
                        VariableDeclarationKind::Var => "var",
                        VariableDeclarationKind::Let => "let",
                        VariableDeclarationKind::Const => "const",
                        _ => "var",
                    },
                    "declarations": d.declarations.iter().map(|dd| self.convert_var_declarator(dd)).collect::<Vec<_>>(),
                })
            }
            Declaration::FunctionDeclaration(d) => {
                self.convert_function(
                    "FunctionDeclaration",
                    d.id.as_ref(),
                    &d.params,
                    d.body.as_deref(),
                    d.r#async,
                    d.generator,
                )
            }
            Declaration::ClassDeclaration(d) => {
                self.convert_class("ClassDeclaration", d.id.as_ref(), &d.body)
            }
            _ => json!({"type": "UnknownDeclaration"}),
        }
    }

    fn convert_function(
        &mut self,
        type_name: &str,
        id: Option<&BindingIdentifier<'_>>,
        params: &FormalParameters<'_>,
        _body: Option<&FunctionBody<'_>>,
        is_async: bool,
        is_generator: bool,
    ) -> Value {
        json!({
            "type": type_name,
            "id": id.map(|i| self.convert_binding_identifier(i)),
            "params": params.items.iter().map(|p| self.convert_formal_parameter(p)).collect::<Vec<_>>(),
            "async": is_async,
            "generator": is_generator,
        })
    }

    fn convert_class(
        &mut self,
        type_name: &str,
        id: Option<&BindingIdentifier<'_>>,
        _body: &ClassBody<'_>,
    ) -> Value {
        json!({
            "type": type_name,
            "id": id.map(|i| self.convert_binding_identifier(i)),
        })
    }
}

impl Visit<'_> for EstreeConverter {
    fn visit_program(&mut self, program: &Program<'_>) {
        for stmt in &program.body {
            let converted = self.convert_statement(stmt);
            eprintln!("pushing: {:?} (is_null={})", converted["type"], converted.is_null());
            self.body.push(converted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc::allocator::Allocator;
    use oxc::parser::{Parser, ParserReturn};
    use oxc::span::SourceType;

    fn parse_to_estree(source: &str) -> Value {
        let allocator = Allocator::default();
        let ParserReturn { program, panicked, .. } =
            Parser::new(&allocator, source, SourceType::mjs()).parse();
        assert!(!panicked, "parser panicked");
        program_to_estree(&program)
    }

    #[test]
    fn debug_parse() {
        let source = "import { foo } from 'bar';";
        let allocator = Allocator::default();
        let ParserReturn { program, panicked, diagnostics, .. } =
            Parser::new(&allocator, source, SourceType::mjs()).parse();
        eprintln!("panicked: {}, diagnostics: {}, body_len: {}", panicked, diagnostics.len(), program.body.len());
        if !diagnostics.is_empty() {
            eprintln!("first diagnostic: {:?}", diagnostics[0]);
        }
        assert_eq!(program.body.len(), 1);
    }

    #[test]
    fn converts_import() {
        let ast = parse_to_estree("import { foo } from 'bar';");
        eprintln!("ast type: {:?}", ast["type"]);
        eprintln!("ast body type: {:?}", ast["body"]);
        eprintln!("ast body[0]: {:?}", ast["body"][0]);
        eprintln!("ast = {}", serde_json::to_string_pretty(&ast).unwrap());
        assert_eq!(ast["type"], "Program");
        assert_eq!(ast["body"][0]["type"], "ImportDeclaration");
        assert_eq!(ast["body"][0]["source"]["value"], "bar");
    }

    #[test]
    fn converts_const() {
        let ast = parse_to_estree("const x = 1;");
        assert_eq!(ast["body"][0]["type"], "VariableDeclaration");
        assert_eq!(ast["body"][0]["kind"], "const");
        assert_eq!(ast["body"][0]["declarations"][0]["id"]["name"], "x");
        assert_eq!(ast["body"][0]["declarations"][0]["init"]["value"], 1.0);
    }

    #[test]
    fn converts_call_expression() {
        let ast = parse_to_estree("console.log('hi');");
        assert_eq!(ast["body"][0]["type"], "ExpressionStatement");
        assert_eq!(ast["body"][0]["expression"]["type"], "CallExpression");
        assert_eq!(ast["body"][0]["expression"]["callee"]["type"], "MemberExpression");
        assert_eq!(ast["body"][0]["expression"]["callee"]["object"]["name"], "console");
        assert_eq!(ast["body"][0]["expression"]["callee"]["property"]["name"], "log");
        assert_eq!(ast["body"][0]["expression"]["callee"]["computed"], false);
    }

    #[test]
    fn converts_export_default_function() {
        let ast = parse_to_estree("export default function Foo() {}");
        assert_eq!(ast["body"][0]["type"], "ExportDefaultDeclaration");
    }

    #[test]
    fn converts_arrow_function() {
        let ast = parse_to_estree("const f = () => 1;");
        assert_eq!(ast["body"][0]["declarations"][0]["init"]["type"], "ArrowFunctionExpression");
        assert_eq!(ast["body"][0]["declarations"][0]["init"]["expression"], true);
    }

    #[test]
    fn converts_array_expression() {
        let ast = parse_to_estree("const arr = [1, 2, 3];");
        let init = &ast["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "ArrayExpression");
        assert_eq!(init["elements"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn converts_object_expression() {
        let ast = parse_to_estree("const obj = { x: 1, y: 2 };");
        let init = &ast["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "ObjectExpression");
        assert_eq!(init["properties"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn converts_dynamic_import() {
        let ast = parse_to_estree("import('foo');");
        assert_eq!(ast["body"][0]["expression"]["type"], "ImportExpression");
        assert_eq!(ast["body"][0]["expression"]["source"]["value"], "foo");
    }

    #[test]
    fn converts_template_literal() {
        let ast = parse_to_estree("const s = `hello ${name}`;");
        let init = &ast["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "TemplateLiteral");
        assert_eq!(init["expressions"].as_array().unwrap().len(), 1);
        assert_eq!(init["quasis"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn converts_binary_expression() {
        let ast = parse_to_estree("const x = 1 + 2;");
        let init = &ast["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "BinaryExpression");
        assert_eq!(init["left"]["value"], 1.0);
        assert_eq!(init["right"]["value"], 2.0);
    }

    #[test]
    fn converts_await_expression() {
        let ast = parse_to_estree("const x = await fetch();");
        let init = &ast["body"][0]["declarations"][0]["init"];
        assert_eq!(init["type"], "AwaitExpression");
    }
}
