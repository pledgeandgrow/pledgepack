//! Static evaluation of JS/TS config files.
//!
//! Vite, webpack, Next.js and pledge configs are ordinary JS/TS modules. We
//! never execute them; instead the file is parsed with Oxc and the exported
//! config object is folded into a [`serde_json::Value`]. Literals become JSON
//! values; anything that needs a runtime (calls, identifiers, `path.resolve`)
//! becomes `{"$call": "name", "$args": [...]}` (calls / `new`) or
//! `{"$expr": "<source text>"}` so callers can still recover names and string
//! arguments.

use anyhow::{Result, anyhow};
use oxc::allocator::Allocator;
use oxc::ast::ast::{
    Argument, ArrayExpressionElement, Expression, ObjectPropertyKind, Program, Statement,
    VariableDeclarationKind,
};
use oxc::parser::{Parser, ParserReturn};
use oxc::span::{GetSpan, SourceType};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Marker keys used for values that could not be evaluated statically.
pub const CALL_KEY: &str = "$call";
pub const ARGS_KEY: &str = "$args";
pub const EXPR_KEY: &str = "$expr";

/// Evaluate the exported config object of `source` (`export default ...`,
/// `module.exports = ...`, `export default defineConfig(...)`, arrow-function
/// configs, or a top-level `const config = {...}` re-exported by name).
pub fn eval_config_module(source: &str, file_name: &str) -> Result<Value> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file_name)
        .unwrap_or_else(|_| SourceType::mjs())
        .with_module(true);
    let ParserReturn {
        program, panicked, ..
    } = Parser::new(&allocator, source, source_type).parse();
    if panicked {
        return Err(anyhow!("could not parse {}", file_name));
    }
    let ev = Evaluator::new(source, &program);
    let expr = ev
        .find_export(&program)
        .ok_or_else(|| anyhow!("no default export / module.exports found in {}", file_name))?;
    let value = ev.eval_config_expr(expr, 0);
    match value {
        Some(Value::Object(_)) => Ok(value.unwrap_or(Value::Null)),
        _ => Err(anyhow!(
            "the exported config in {} is not a static object",
            file_name
        )),
    }
}

struct Evaluator<'a, 'b> {
    source: &'a str,
    /// Top-level `const name = <expr>` bindings, for `export default name`.
    bindings: HashMap<String, &'b Expression<'a>>,
}

impl<'a, 'b> Evaluator<'a, 'b> {
    fn new(source: &'a str, program: &'b Program<'a>) -> Self {
        let mut bindings = HashMap::new();
        for stmt in &program.body {
            if let Statement::VariableDeclaration(v) = stmt
                && matches!(
                    v.kind,
                    VariableDeclarationKind::Const
                        | VariableDeclarationKind::Let
                        | VariableDeclarationKind::Var
                )
            {
                for d in &v.declarations {
                    if let (Some(id), Some(init)) = (d.id.get_binding_identifier(), d.init.as_ref())
                    {
                        bindings.insert(id.name.to_string(), init);
                    }
                }
            }
        }
        Self { source, bindings }
    }

    fn text(&self, span: oxc::span::Span) -> &'a str {
        self.source
            .get(span.start as usize..span.end as usize)
            .unwrap_or("")
    }

    fn find_export(&self, program: &'b Program<'a>) -> Option<&'b Expression<'a>> {
        for stmt in &program.body {
            match stmt {
                Statement::ExportDefaultDeclaration(d) => {
                    if let Some(e) = d.declaration.as_expression() {
                        return Some(e);
                    }
                }
                Statement::ExpressionStatement(s) => {
                    if let Expression::AssignmentExpression(a) = s.expression.get_inner_expression()
                    {
                        let left = self.text(a.left.span());
                        if left == "module.exports" || left == "exports.default" {
                            return Some(&a.right);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Unwrap wrappers (`defineConfig(x)`, `(env) => x`, identifiers) down to
    /// the config object and evaluate it.
    fn eval_config_expr(&self, expr: &Expression<'a>, depth: usize) -> Option<Value> {
        if depth > 8 {
            return None;
        }
        let expr = expr.get_inner_expression();
        match expr {
            Expression::ObjectExpression(_) => Some(self.eval(expr)),
            Expression::Identifier(id) => {
                let target = self.bindings.get(id.name.as_str())?;
                self.eval_config_expr(target, depth + 1)
            }
            Expression::CallExpression(call) => {
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression()
                        && let Some(v) = self.eval_config_expr(e, depth + 1)
                        && v.is_object()
                    {
                        return Some(v);
                    }
                }
                None
            }
            Expression::ArrowFunctionExpression(f) => {
                for stmt in &f.body.statements {
                    match stmt {
                        Statement::ExpressionStatement(s) if f.expression => {
                            return self.eval_config_expr(&s.expression, depth + 1);
                        }
                        Statement::ReturnStatement(r) => {
                            if let Some(arg) = &r.argument {
                                return self.eval_config_expr(arg, depth + 1);
                            }
                        }
                        _ => {}
                    }
                }
                None
            }
            Expression::FunctionExpression(f) => {
                let body = f.body.as_ref()?;
                for stmt in &body.statements {
                    if let Statement::ReturnStatement(r) = stmt
                        && let Some(arg) = &r.argument
                    {
                        return self.eval_config_expr(arg, depth + 1);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn eval(&self, expr: &Expression<'a>) -> Value {
        let expr = expr.get_inner_expression();
        match expr {
            Expression::StringLiteral(s) => Value::String(s.value.to_string()),
            Expression::NumericLiteral(n) => serde_json::Number::from_f64(n.value)
                .map(|n| {
                    if n.as_f64()
                        .is_some_and(|f| f.fract() == 0.0 && f.abs() < 9e15)
                    {
                        Value::Number(serde_json::Number::from(n.as_f64().unwrap_or(0.0) as i64))
                    } else {
                        Value::Number(n)
                    }
                })
                .unwrap_or(Value::Null),
            Expression::BooleanLiteral(b) => Value::Bool(b.value),
            Expression::NullLiteral(_) => Value::Null,
            Expression::Identifier(id) if id.name == "undefined" => Value::Null,
            Expression::TemplateLiteral(t) if t.expressions.is_empty() => Value::String(
                t.quasis
                    .iter()
                    .filter_map(|q| q.value.cooked.as_ref().map(|c| c.to_string()))
                    .collect(),
            ),
            Expression::UnaryExpression(u) => match (u.operator.as_str(), self.eval(&u.argument)) {
                ("-", Value::Number(n)) => n
                    .as_i64()
                    .map(|i| Value::Number((-i).into()))
                    .or_else(|| {
                        n.as_f64()
                            .and_then(|f| serde_json::Number::from_f64(-f))
                            .map(Value::Number)
                    })
                    .unwrap_or(Value::Null),
                ("!", Value::Bool(b)) => Value::Bool(!b),
                _ => self.unevaluated(expr),
            },
            Expression::ObjectExpression(o) => {
                let mut map = Map::new();
                for prop in &o.properties {
                    if let ObjectPropertyKind::ObjectProperty(p) = prop
                        && let Some(name) = p.key.static_name()
                    {
                        map.insert(name.to_string(), self.eval(&p.value));
                    }
                    // Spread elements / computed keys are not statically known.
                }
                Value::Object(map)
            }
            Expression::ArrayExpression(a) => Value::Array(
                a.elements
                    .iter()
                    .filter_map(|el| match el {
                        ArrayExpressionElement::SpreadElement(_)
                        | ArrayExpressionElement::Elision(_) => None,
                        other => other.as_expression().map(|e| self.eval(e)),
                    })
                    .collect(),
            ),
            Expression::CallExpression(c) => {
                let mut m = Map::new();
                m.insert(
                    CALL_KEY.into(),
                    Value::String(self.text(c.callee.span()).to_string()),
                );
                m.insert(ARGS_KEY.into(), self.eval_args(&c.arguments));
                Value::Object(m)
            }
            Expression::NewExpression(c) => {
                let mut m = Map::new();
                m.insert(
                    CALL_KEY.into(),
                    Value::String(self.text(c.callee.span()).to_string()),
                );
                m.insert(ARGS_KEY.into(), self.eval_args(&c.arguments));
                Value::Object(m)
            }
            Expression::Identifier(id) => match self.bindings.get(id.name.as_str()) {
                Some(target) if !matches!(target, Expression::Identifier(_)) => self.eval(target),
                _ => self.unevaluated(expr),
            },
            _ => self.unevaluated(expr),
        }
    }

    fn eval_args(&self, args: &[Argument<'a>]) -> Value {
        Value::Array(
            args.iter()
                .filter_map(|a| a.as_expression().map(|e| self.eval(e)))
                .collect(),
        )
    }

    fn unevaluated(&self, expr: &Expression<'a>) -> Value {
        let mut m = Map::new();
        m.insert(
            EXPR_KEY.into(),
            Value::String(self.text(expr.span()).to_string()),
        );
        Value::Object(m)
    }
}

/// Whether `v` is an un-evaluated placeholder (`$call` / `$expr`).
pub fn is_dynamic(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.contains_key(CALL_KEY) || o.contains_key(EXPR_KEY))
}

/// Best-effort string for a value: a literal string, or the last string literal
/// found in an un-evaluated expression such as `path.resolve(__dirname, 'src')`.
pub fn as_path_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => {
            if let Some(args) = o.get(ARGS_KEY).and_then(|a| a.as_array()) {
                return args.iter().rev().find_map(|a| a.as_str().map(String::from));
            }
            let text = o.get(EXPR_KEY)?.as_str()?;
            last_string_literal(text)
        }
        _ => None,
    }
}

fn last_string_literal(text: &str) -> Option<String> {
    let mut last = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'\'' | b'"' | b'`') {
            let q = bytes[i];
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != q {
                j += 1;
            }
            if j <= bytes.len() {
                last = Some(text[start..j.min(bytes.len())].to_string());
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    last
}

/// Name of the callee of a `$call` placeholder (`react` for `react()`).
pub fn call_name(v: &Value) -> Option<&str> {
    v.as_object()?.get(CALL_KEY)?.as_str()
}

/// Leading identifier of an un-evaluated expression (`react` in `react({..})`).
pub fn expr_head(v: &Value) -> Option<String> {
    let text = v.as_object()?.get(EXPR_KEY)?.as_str()?;
    let head: String = text
        .trim()
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | '-'))
        .collect();
    (!head.is_empty()).then_some(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn evaluates_define_config_object() {
        let src = r#"
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
export default defineConfig({
  plugins: [react()],
  server: { port: 5199, open: true },
  resolve: { alias: { '@': '/src' } },
  build: { outDir: 'build', sourcemap: true, target: 'es2020' },
  define: { __APP__: '"x"' },
})
"#;
        let v = eval_config_module(src, "vite.config.ts").unwrap();
        assert_eq!(v["server"]["port"], json!(5199));
        assert_eq!(v["build"]["outDir"], json!("build"));
        assert_eq!(v["resolve"]["alias"]["@"], json!("/src"));
        assert_eq!(v["define"]["__APP__"], json!("\"x\""));
        assert_eq!(call_name(&v["plugins"][0]), Some("react"));
    }

    #[test]
    fn handles_function_configs_module_exports_and_named_consts() {
        let v = eval_config_module(
            "export default defineConfig(({ mode }) => ({ base: '/a/' }))",
            "vite.config.ts",
        )
        .unwrap();
        assert_eq!(v["base"], json!("/a/"));

        let v = eval_config_module(
            "const path = require('path');\nmodule.exports = { entry: './src/i.js', output: { path: path.resolve(__dirname, 'dist') } };",
            "webpack.config.js",
        )
        .unwrap();
        assert_eq!(
            as_path_string(&v["output"]["path"]).as_deref(),
            Some("dist")
        );

        let v = eval_config_module(
            "const config = { a: -1, b: [1, 'x', null] };\nexport default config;",
            "x.config.mjs",
        )
        .unwrap();
        assert_eq!(v["a"], json!(-1));
        assert_eq!(v["b"], json!([1, "x", null]));
    }

    #[test]
    fn unparseable_or_exportless_files_error_instead_of_hanging() {
        assert!(eval_config_module("const x = ;", "a.ts").is_err());
        assert!(eval_config_module("const x = 1;", "a.ts").is_err());
    }
}
