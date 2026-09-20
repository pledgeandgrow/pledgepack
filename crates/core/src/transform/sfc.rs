// Single-File Component transforms: Vue, Svelte, Astro

use super::TransformOutput;
use anyhow::Result;
use oxc::allocator::Allocator;
use oxc::codegen::Codegen;
use oxc::parser::{Parser, ParserReturn};
use oxc::span::SourceType;
use oxc::transformer::{TransformOptions, Transformer};
use std::path::Path;

// ─── Vue SFC Parser ──────────────────────────────────────────────────

/// Transform a Vue Single-File Component (.vue)
/// Extracts <template>, <script setup>, and <style> blocks
/// Produces a JS module with render function + component options
pub(super) fn transform_vue(
    source: &str,
    file_path: &str,
    is_production: bool,
) -> Result<TransformOutput> {
    let template = extract_sfc_block(source, "template");
    let script = extract_sfc_block(source, "script");
    let style = extract_sfc_block(source, "style");
    let style_scoped = source.contains("<style scoped");

    let mut code = String::new();
    let mut extracted_css = None;

    if let Some(style_content) = &style {
        let css = if style_scoped {
            add_scope_to_css(style_content, "data-v-pledge")
        } else {
            style_content.clone()
        };
        extracted_css = Some(css);
    }

    if let Some(script_content) = &script {
        let is_setup = source.contains("<script setup");
        let is_ts =
            source.contains("<script setup lang=\"ts\"") || source.contains("<script lang=\"ts\"");

        let transformed_script = if is_ts {
            let allocator = Allocator::default();
            let source_type = SourceType::tsx();
            let ParserReturn {
                mut program,
                panicked,
                ..
            } = Parser::new(&allocator, script_content, source_type).parse();
            if !panicked {
                let mut options = TransformOptions::default();
                options.typescript.only_remove_type_imports = false;
                let semantic = oxc::semantic::SemanticBuilder::new()
                    .with_check_syntax_error(false)
                    .build(&program);
                let transformer = Transformer::new(&allocator, Path::new(file_path), &options);
                let scoping = semantic.semantic.into_scoping();
                let _ = transformer.build_with_scoping(scoping, &mut program);
                let result = Codegen::new().build(&program);
                result.code
            } else {
                script_content.clone()
            }
        } else {
            script_content.clone()
        };

        if is_setup {
            code.push_str("// Vue SFC (script setup) — compiled by Pledge\n");
            code.push_str(&transformed_script);
            code.push('\n');
            if let Some(template_content) = &template {
                let render_fn = compile_vue_template(template_content);
                code.push_str(&format!(
                    "\nexport default {{\n  render: {}\n}};\n",
                    render_fn
                ));
            } else {
                code.push_str("\nexport default {};\n");
            }
        } else {
            code.push_str("// Vue SFC — compiled by Pledge\n");
            code.push_str(&transformed_script);
            code.push('\n');
            if let Some(template_content) = &template {
                let render_fn = compile_vue_template(template_content);
                code = code.replace(
                    "export default {",
                    &format!("export default {{\n  render: {},\n", render_fn),
                );
            }
        }
    } else if let Some(template_content) = &template {
        let render_fn = compile_vue_template(template_content);
        code.push_str(&format!(
            "// Vue SFC — compiled by Pledge\nexport default {{\n  render: {}\n}};\n",
            render_fn
        ));
    } else {
        code.push_str("// Vue SFC — empty\nexport default {};\n");
    }

    if !is_production {
        code.push_str(
            r#"
// Vue HMR — component-level hot replacement
if (import.meta.hot) {
  const __vue_component = __pledge_vue_components && __pledge_vue_components['"#,
        );
        code.push_str(file_path);
        code.push_str(
            r#"'];
  if (__vue_component && __vue_component.__hmr_id) {
    import.meta.hot.accept((newModule) => {
      if (newModule && newModule.default) {
        // Swap render function on existing component instances
        const newRender = newModule.default.render;
        if (newRender) {
          __vue_component.render = newRender;
          // Force re-render of all mounted instances
          if (__vue_component.__instances) {
            __vue_component.__instances.forEach(instance => {
              if (instance && instance.forceUpdate) {
                instance.forceUpdate();
              }
            });
          }
        }
      }
    });
  }
  import.meta.hot.accept();
}
"#,
        );
    }

    let source_map = Some(super::utils::generate_source_map(file_path, source, &code));

    Ok(TransformOutput {
        code,
        source_map,
        css_modules: None,
        is_css: false,
        extracted_css,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}

/// Extract a named block from an SFC (Vue/Svelte)
/// e.g., extract_sfc_block(source, "template") returns content between <template> and </template>
///
/// This implementation tracks nesting depth so that nested tags of the same name
/// (e.g. `<template>` inside `<template>`) and tags with attributes
/// (e.g. `<template lang="pug">`) are handled correctly. It returns the first
/// matching block. Use [`extract_sfc_blocks`] to retrieve *all* blocks of a
/// given type (e.g. multiple `<style>` blocks).
fn extract_sfc_block(source: &str, tag: &str) -> Option<String> {
    extract_sfc_blocks(source, tag)
        .into_iter()
        .next()
        .map(|(_, content)| content)
}

/// Extract *all* named blocks of a given tag from an SFC source.
///
/// Each entry is `(lang, content)` where `lang` is the value of the optional
/// `lang="..."` attribute on the opening tag (e.g. `"ts"`, `"pug"`).
fn extract_sfc_blocks(source: &str, tag: &str) -> Vec<(Option<String>, String)> {
    let mut blocks = Vec::new();
    let open_prefix = format!("<{}", tag);
    let close_tag = format!("</{}>", tag);

    let mut search_from = 0;
    while let Some((_open_start, content_start, is_self_closing, lang)) =
        find_open_tag(source, search_from, &open_prefix)
    {
        // Self-closing tags (e.g. `<style />`) have no content; skip them.
        if is_self_closing {
            search_from = content_start;
            continue;
        }

        // Find the matching closing tag, tracking nesting depth for tags of
        // the same name so that nested occurrences don't terminate the block
        // prematurely.
        let mut depth: isize = 1;
        let mut pos = content_start;
        let mut found_close = None;
        while depth > 0 && pos < source.len() {
            let next_open =
                find_open_tag(source, pos, &open_prefix).map(|(s, cs, sc, _)| (s, cs, sc));
            let next_close = source[pos..].find(&close_tag).map(|p| pos + p);
            match (next_open, next_close) {
                (Some((op, cs, sc)), Some(cl)) if op < cl => {
                    if sc {
                        // Self-closing nested tag: doesn't affect depth.
                        pos = cs;
                    } else {
                        depth += 1;
                        pos = op + open_prefix.len();
                    }
                }
                (Some((op, cs, sc)), None) => {
                    if sc {
                        pos = cs;
                    } else {
                        depth += 1;
                        pos = op + open_prefix.len();
                    }
                }
                (_, Some(cl)) => {
                    depth -= 1;
                    if depth == 0 {
                        found_close = Some(cl);
                        break;
                    } else {
                        pos = cl + close_tag.len();
                    }
                }
                (None, None) => break,
            }
        }

        if let Some(cl) = found_close {
            let content = &source[content_start..cl];
            blocks.push((lang, content.trim().to_string()));
            search_from = cl + close_tag.len();
        } else {
            // No matching close tag found; stop scanning.
            break;
        }
    }
    blocks
}

/// Find the next opening tag matching `open_prefix` (e.g. `<template`) starting
/// from `from`. Returns `(open_start, content_start, is_self_closing, lang)`
/// where `content_start` is the index just after the closing `>` of the opening
/// tag. Skips false matches where the prefix is part of a longer tag name
/// (e.g. `<templatex>`).
fn find_open_tag(
    source: &str,
    from: usize,
    open_prefix: &str,
) -> Option<(usize, usize, bool, Option<String>)> {
    let mut search = from;
    loop {
        let rel = source[search..].find(open_prefix)?;
        let abs = search + rel;
        // Boundary check: the character right after the prefix must not be
        // alphanumeric, otherwise this is a different tag (e.g. `<templatex>`).
        let after = &source[abs + open_prefix.len()..];
        if let Some(c) = after.chars().next()
            && c.is_alphanumeric()
        {
            search = abs + open_prefix.len();
            continue;
        }
        // Find the end of the opening tag (the next `>`).
        let gt = source[abs..].find('>')?;
        let tag_end = abs + gt + 1;
        let tag_str = &source[abs..tag_end];
        let is_self_closing = tag_str.ends_with("/>");
        let lang = extract_lang_attr(tag_str);
        return Some((abs, tag_end, is_self_closing, lang));
    }
}

/// Extract the `lang="..."` attribute value from an opening tag string.
fn extract_lang_attr(tag: &str) -> Option<String> {
    if let Some(idx) = tag.find("lang=\"") {
        let after = &tag[idx + "lang=\"".len()..];
        if let Some(end) = after.find('"') {
            return Some(after[..end].to_string());
        }
    }
    if let Some(idx) = tag.find("lang='") {
        let after = &tag[idx + "lang='".len()..];
        if let Some(end) = after.find('\'') {
            return Some(after[..end].to_string());
        }
    }
    None
}

/// Compile a Vue template string to a render function using h() calls.
/// Parses HTML-like templates and generates Vue 3 render functions with:
/// - Tag nesting (div > span > text)
/// - Attributes (class, style, id, data-*)
/// - Vue directives: v-if, v-else, v-for, v-bind (:), v-on (@), v-model, v-show, v-text, v-html
/// - Mustache interpolation {{ expr }}
/// - Self-closing tags
/// - HTML entities
fn compile_vue_template(template: &str) -> String {
    let nodes = parse_html_template(template);
    let body = nodes_to_render_calls(&nodes, 0);
    if body.is_empty() {
        return "function render() { return null; }".to_string();
    }
    format!("function render() {{\n  return {};\n}}", body)
}

/// A parsed HTML node (element or text)
#[derive(Debug, Clone)]
enum HtmlNode {
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        children: Vec<HtmlNode>,
        #[allow(dead_code)]
        self_closing: bool,
    },
    Text(String),
}

/// Parse an HTML template string into a tree of HtmlNode
fn parse_html_template(html: &str) -> Vec<HtmlNode> {
    let trimmed = html.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let mut parser = HtmlParser::new(trimmed);
    parser.parse_children()
}

struct HtmlParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> HtmlParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.input.len());
    }

    fn starts_with(&self, s: &str) -> bool {
        self.remaining().starts_with(s)
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance(1);
            } else {
                break;
            }
        }
    }

    fn parse_children(&mut self) -> Vec<HtmlNode> {
        let mut nodes = vec![];
        loop {
            self.skip_whitespace();
            if self.peek().is_none() {
                break;
            }
            if self.starts_with("</") {
                break;
            }
            if self.starts_with("<!--") {
                let end = self
                    .remaining()
                    .find("-->")
                    .unwrap_or(self.remaining().len());
                self.advance(end + 3);
                continue;
            }
            if self.starts_with("<") {
                if let Some(node) = self.parse_element() {
                    nodes.push(node);
                }
            } else {
                let text = self.parse_text();
                if !text.trim().is_empty() {
                    nodes.push(HtmlNode::Text(text.trim().to_string()));
                }
            }
        }
        nodes
    }

    fn parse_element(&mut self) -> Option<HtmlNode> {
        self.advance(1); // skip <
        let tag = self.parse_tag_name()?;
        let mut attrs = vec![];
        let mut self_closing = false;

        loop {
            self.skip_whitespace();
            self.peek()?;
            if self.starts_with("/>") {
                self.advance(2);
                self_closing = true;
                break;
            }
            if self.starts_with(">") {
                self.advance(1);
                break;
            }
            if let Some((name, value)) = self.parse_attribute() {
                attrs.push((name, value));
            }
        }

        let children = if self_closing {
            vec![]
        } else {
            let children = self.parse_children();
            if self.starts_with("</") {
                let close_end = self.remaining().find('>').unwrap_or(self.remaining().len());
                self.advance(close_end + 1);
            }
            children
        };

        Some(HtmlNode::Element {
            tag,
            attrs,
            children,
            self_closing,
        })
    }

    fn parse_tag_name(&mut self) -> Option<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '-' || c == ':' {
                self.advance(1);
            } else {
                break;
            }
        }
        if self.pos == start {
            None
        } else {
            Some(self.input[start..self.pos].to_string())
        }
    }

    fn parse_attribute(&mut self) -> Option<(String, String)> {
        let name = self.parse_attr_name()?;
        self.skip_whitespace();
        if self.starts_with("=") {
            self.advance(1);
            self.skip_whitespace();
            let value = self.parse_attr_value();
            Some((name, value))
        } else {
            Some((name, "true".to_string()))
        }
    }

    fn parse_attr_name(&mut self) -> Option<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '-' || c == ':' || c == '@' || c == '.' || c == '*' {
                self.advance(1);
            } else {
                break;
            }
        }
        if self.pos == start {
            None
        } else {
            Some(self.input[start..self.pos].to_string())
        }
    }

    fn parse_attr_value(&mut self) -> String {
        let quote = self.peek().filter(|c| *c == '"' || *c == '\'');
        if let Some(q) = quote {
            self.advance(1);
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c == q {
                    break;
                }
                self.advance(1);
            }
            let value = self.input[start..self.pos].to_string();
            if self.peek() == Some(q) {
                self.advance(1);
            }
            value
        } else {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_whitespace() || c == '>' || c == '/' {
                    break;
                }
                self.advance(1);
            }
            self.input[start..self.pos].to_string()
        }
    }

    fn parse_text(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '<' {
                break;
            }
            self.advance(1);
        }
        self.input[start..self.pos].to_string()
    }
}

/// Convert parsed HTML nodes to Vue h() render calls.
///
/// Handles `v-if` / `v-else` pairs at the sibling level by emitting ternary
/// expressions (`cond ? trueBranch : falseBranch`).
fn nodes_to_render_calls(nodes: &[HtmlNode], depth: usize) -> String {
    if nodes.len() == 1 {
        return node_to_render_call_opts(&nodes[0], depth, false);
    }
    let mut items: Vec<String> = vec![];
    let mut i = 0;
    while i < nodes.len() {
        let node = &nodes[i];
        if let HtmlNode::Element { attrs, .. } = node
            && has_directive(attrs, "v-if")
        {
            let cond = get_directive(attrs, "v-if").unwrap_or_default();
            // Look ahead for a `v-else` sibling to form an else branch.
            if i + 1 < nodes.len()
                && let HtmlNode::Element {
                    attrs: next_attrs, ..
                } = &nodes[i + 1]
                && has_directive(next_attrs, "v-else")
            {
                // Render the true branch without re-applying its own
                // v-if ternary (the pair forms the ternary here).
                let true_expr = node_to_render_call_opts(node, depth + 1, true);
                let false_expr = node_to_render_call_opts(&nodes[i + 1], depth + 1, false);
                items.push(format!("({}) ? {} : {}", cond, true_expr, false_expr));
                i += 2;
                continue;
            }
            // No v-else sibling: render with a `null` else branch.
            let true_expr = node_to_render_call_opts(node, depth + 1, false);
            items.push(format!("({}) ? {} : null", cond, true_expr));
            i += 1;
            continue;
        }
        items.push(node_to_render_call_opts(node, depth + 1, false));
        i += 1;
    }
    format!("[{}]", items.join(", "))
}

// PRODUCTION-READINESS-100.md goal 91: `node_to_render_call`, a thin
// zero-call-site wrapper around `node_to_render_call_opts`, was removed —
// every real call site already calls `node_to_render_call_opts` directly.

/// Convert a single HTML node to a Vue h() call.
///
/// `skip_vif` is used when the caller is already building a v-if/v-else ternary
/// and wants to suppress the node's own `? ... : null` wrapping for the true
/// branch.
///
/// Directives handled here:
/// - `v-if`: wraps the h() call in `(cond) ? h(...) : null`.
/// - `v-for`: wraps the h() call in `list.map(item => h(...))`.
fn node_to_render_call_opts(node: &HtmlNode, depth: usize, skip_vif: bool) -> String {
    let indent = "  ".repeat(depth);
    match node {
        HtmlNode::Text(text) => {
            if text.contains("{{") {
                render_mustache(text, &indent)
            } else {
                format!("'{}'", escape_js_string(text))
            }
        }
        HtmlNode::Element {
            tag,
            attrs,
            children,
            ..
        } => {
            let tag_expr = if tag
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                tag.clone()
            } else {
                format!("'{}'", tag)
            };

            let props = attrs_to_props(attrs, tag, &indent);
            let children_expr = if children.is_empty() {
                String::new()
            } else {
                format!(", {}", nodes_to_render_calls(children, depth + 1))
            };

            let base = format!("h({}, {}{})", tag_expr, props, children_expr);

            let vif = get_directive(attrs, "v-if");
            let vfor = get_directive(attrs, "v-for");

            let mut result = base;
            // v-if wraps the element in a ternary (unless the caller is
            // already forming a v-if/v-else pair).
            if !skip_vif && let Some(cond) = &vif {
                result = format!("({}) ? {} : null", cond, result);
            }
            // v-for wraps the (possibly v-if'd) element in a `.map()` call so
            // each item is rendered: `list.map(item => ...)`.
            if let Some(vfor_val) = &vfor {
                let (binding, list_expr) = parse_v_for(vfor_val);
                result = format!("{}.map({} => {})", list_expr, binding, result);
            }
            result
        }
    }
}

/// Return the value of a directive attribute, if present.
fn get_directive(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.clone())
}

/// Whether a directive attribute is present.
fn has_directive(attrs: &[(String, String)], name: &str) -> bool {
    attrs.iter().any(|(n, _)| n == name)
}

/// Parse a `v-for` value into `(item_binding, list_expr)`.
///
/// Supports the common forms:
/// - `"item in items"`
/// - `"(item, index) in items"`
/// - `"item, index in items"`
fn parse_v_for(value: &str) -> (String, String) {
    let trimmed = value.trim();
    if let Some(idx) = trimmed.find(" in ") {
        let lhs = trimmed[..idx].trim();
        let rhs = trimmed[idx + 4..].trim();
        let binding = if lhs.starts_with('(') && lhs.ends_with(')') {
            lhs[1..lhs.len() - 1].trim().to_string()
        } else {
            lhs.to_string()
        };
        (binding, rhs.to_string())
    } else {
        // Fallback: treat the whole expression as the item binding with an
        // empty list so output still type-checks.
        (trimmed.to_string(), "[]".to_string())
    }
}

/// Convert HTML attributes to Vue props object.
///
/// `tag` is the element tag name, used to branch `v-model` behavior across
/// input types (checkbox, select, component, default text input).
fn attrs_to_props(attrs: &[(String, String)], tag: &str, _indent: &str) -> String {
    let mut props: Vec<String> = vec![];

    for (name, value) in attrs {
        // Structural directives are handled in `node_to_render_call_opts`;
        // skip them here so they don't leak into the props object.
        if name == "v-if" || name == "v-else" || name == "v-for" {
            continue;
        } else if name == "v-show" {
            props.push(format!("style: {{ display: ({} ? '' : 'none') }}", value));
        } else if name == "v-text" {
            props.push(format!("textContent: {}", value));
        } else if name == "v-html" {
            props.push(format!("innerHTML: {}", value));
        } else if name == "v-model" || name.starts_with("v-model.") {
            push_v_model_props(&mut props, name, value, tag, attrs);
        } else if name.starts_with(':') || name.starts_with("v-bind:") {
            let prop_name = name.trim_start_matches(':').trim_start_matches("v-bind:");
            if prop_name == "class" {
                props.push(format!("class: {}", value));
            } else if prop_name == "style" {
                props.push(format!("style: {}", value));
            } else if prop_name == "key" {
                props.push(format!("key: {}", value));
            } else if prop_name == "ref" {
                props.push(format!("ref: {}", value));
            } else {
                props.push(format!("{}: {}", prop_name, value));
            }
        } else if name.starts_with('@') || name.starts_with("v-on:") {
            let event = name.trim_start_matches('@').trim_start_matches("v-on:");
            let handler = if value.contains("(") {
                value.clone()
            } else {
                format!("() => {}()", value)
            };
            props.push(format!("on{}: {}", capitalize(event), handler));
        } else if name == "class" {
            props.push(format!("class: '{}'", escape_js_string(value)));
        } else if name == "style" {
            let style_obj = css_string_to_object(value);
            props.push(format!("style: {}", style_obj));
        } else if name == "key" || name == "ref" {
            props.push(format!("{}: '{}'", name, escape_js_string(value)));
        } else if name.starts_with("data-") || name.starts_with("aria-") {
            props.push(format!("'{}': '{}'", name, escape_js_string(value)));
        } else {
            props.push(format!("{}: '{}'", name, escape_js_string(value)));
        }
    }

    if props.is_empty() {
        return "{}".to_string();
    }

    format!("{{ {} }}", props.join(", "))
}

/// Push props for a `v-model` directive, branching on element type and
/// modifiers.
///
/// Element types:
/// - Component (PascalCase tag): `modelValue` + `onUpdate:modelValue`.
/// - `<input type="checkbox">`: `checked` + `onChange` (e.target.checked).
/// - `<select>`: `value` + `onChange` (e.target.value).
/// - Default (text input / textarea): `value` + `onInput` (e.target.value).
///
/// Modifiers:
/// - `.number`: coerce with `Number(...)`.
/// - `.trim`: coerce with `.trim()`.
/// - `.lazy`: use `onChange` instead of `onInput`.
fn push_v_model_props(
    props: &mut Vec<String>,
    name: &str,
    value: &str,
    tag: &str,
    attrs: &[(String, String)],
) {
    let modifiers_str = name.strip_prefix("v-model").unwrap_or("");
    let modifiers: Vec<&str> = modifiers_str
        .trim_start_matches('.')
        .split('.')
        .filter(|s| !s.is_empty())
        .collect();
    let is_number = modifiers.contains(&"number");
    let is_trim = modifiers.contains(&"trim");
    let is_lazy = modifiers.contains(&"lazy");

    let event_name = if is_lazy { "onChange" } else { "onInput" };
    let mut val_expr = "e.target.value".to_string();
    if is_trim {
        val_expr = format!("{}.trim()", val_expr);
    }
    if is_number {
        val_expr = format!("Number({})", val_expr);
    }

    let is_component = tag
        .chars()
        .next()
        .map(|c| c.is_uppercase())
        .unwrap_or(false);
    let is_checkbox = tag == "input" && attrs.iter().any(|(n, v)| n == "type" && v == "checkbox");
    let is_select = tag == "select";

    if is_component {
        props.push(format!(
            "modelValue: {}, 'onUpdate:modelValue': (v) => {{ {} = v }}",
            value, value
        ));
    } else if is_checkbox {
        props.push(format!(
            "checked: {}, onChange: (e) => {{ {} = e.target.checked }}",
            value, value
        ));
    } else if is_select {
        props.push(format!(
            "value: {}, onChange: (e) => {{ {} = {} }}",
            value, value, val_expr
        ));
    } else {
        props.push(format!(
            "value: {}, {}: (e) => {{ {} = {} }}",
            value, event_name, value, val_expr
        ));
    }
}

/// Handle Vue mustache interpolation {{ expr }}
fn render_mustache(text: &str, _indent: &str) -> String {
    let mut parts = vec![];
    let mut remaining = text;
    while let Some(start) = remaining.find("{{") {
        if start > 0 {
            let literal = &remaining[..start];
            if !literal.trim().is_empty() {
                parts.push(format!("'{}'", escape_js_string(literal.trim())));
            }
        }
        let after_open = &remaining[start + 2..];
        if let Some(end) = after_open.find("}}") {
            let expr = after_open[..end].trim();
            parts.push(format!("({})", expr));
            remaining = &after_open[end + 2..];
        } else {
            break;
        }
    }
    if !remaining.trim().is_empty() {
        parts.push(format!("'{}'", escape_js_string(remaining.trim())));
    }
    if parts.len() == 1 {
        parts[0].clone()
    } else {
        format!("[{}]", parts.join(", "))
    }
}

/// Escape a string for use in JS single-quoted string
fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Capitalize first letter
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Convert inline CSS string (e.g., "color: red; font-size: 14px") to JS object
fn css_string_to_object(css: &str) -> String {
    let mut pairs = vec![];
    for decl in css.split(';') {
        let decl = decl.trim();
        if let Some(colon) = decl.find(':') {
            let prop = decl[..colon].trim();
            let val = decl[colon + 1..].trim();
            let js_prop = prop.replace('-', "_").to_lowercase();
            pairs.push(format!("{}: '{}'", js_prop, escape_js_string(val)));
        }
    }
    format!("{{ {} }}", pairs.join(", "))
}

/// Add scoped attribute to CSS selectors (for Vue scoped styles)
fn add_scope_to_css(css: &str, attr: &str) -> String {
    let mut result = String::new();
    for line in css.lines() {
        if line.contains('{') && !line.starts_with('@') && !line.starts_with('}') {
            let modified = line.replace("{", &format!("[{}] {{", attr));
            result.push_str(&modified);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

// ─── Svelte Parser ───────────────────────────────────────────────────

/// Transform a Svelte component (.svelte)
/// Extracts <script>, <style>, and markup
/// Produces a JS module with a Svelte-compatible component
pub(super) fn transform_svelte(
    source: &str,
    file_path: &str,
    is_production: bool,
) -> Result<TransformOutput> {
    let script = extract_sfc_block(source, "script");
    let style = extract_sfc_block(source, "style");
    let markup = extract_svelte_markup(source);

    let mut code = String::new();
    let mut extracted_css = None;

    if let Some(style_content) = &style {
        let is_scoped = source.contains("<style scoped");
        let css = if is_scoped {
            add_scope_to_css(style_content, "svelte-pledge")
        } else {
            style_content.clone()
        };
        extracted_css = Some(css);
    }

    code.push_str("// Svelte component — compiled by Pledge\n");

    if let Some(script_content) = &script {
        let is_ts = source.contains("<script lang=\"ts\"");
        let transformed_script = if is_ts {
            let allocator = Allocator::default();
            let ParserReturn {
                mut program,
                panicked,
                ..
            } = Parser::new(&allocator, script_content, SourceType::ts()).parse();
            if !panicked {
                let mut options = TransformOptions::default();
                options.typescript.only_remove_type_imports = false;
                let semantic = oxc::semantic::SemanticBuilder::new()
                    .with_check_syntax_error(false)
                    .build(&program);
                let transformer = Transformer::new(&allocator, Path::new(file_path), &options);
                let scoping = semantic.semantic.into_scoping();
                let _ = transformer.build_with_scoping(scoping, &mut program);
                Codegen::new().build(&program).code
            } else {
                script_content.clone()
            }
        } else {
            script_content.clone()
        };
        // Known limitation (see docs/LIMITATIONS.md, "Svelte SFC support"): script-level
        // reactivity is passed through untransformed. The following are not compiled
        // and rely on a runtime shim:
        // - `$:` reactive statements (should be collected into an update fn)
        // - `$store` auto-subscription (should lower to `store.subscribe()`)
        // - `onMount` / `onDestroy` / lifecycle hooks (passed through as-is)
        code.push_str(&transformed_script);
        code.push('\n');
    }

    let nodes = parse_html_template(&markup);
    let render_body = nodes_to_svelte_render(&nodes, 2);

    code.push_str(&format!(
        r#"
// Svelte component — compiled by Pledge
function create_fragment(ctx) {{
  let root;
{render_body}
  return {{
    mount(target) {{
      target.appendChild(root);
    }},
    destroy() {{
      if (root && root.parentNode) root.parentNode.removeChild(root);
    }}
  }};
}}

export default {{
  create_fragment,
  mount(target, props) {{
    const ctx = {{ ...props }};
    const frag = create_fragment(ctx);
    frag.mount(target);
    return frag;
  }}
}};
"#,
        render_body = render_body
    ));

    if !is_production {
        code.push_str(
            r#"
// Svelte HMR — component-level hot replacement
if (import.meta.hot) {
  import.meta.hot.accept((newModule) => {
    if (newModule && newModule.default) {
      // Find all mounted Svelte components and replace them
      const __svelte_registry = window.__pledge_svelte_components;
      if (__svelte_registry) {
        for (const key of Object.keys(__svelte_registry)) {
          const entry = __svelte_registry[key];
          if (entry && entry.component === __pledge_current_component) {
            // Destroy old component
            if (entry.fragment && entry.fragment.destroy) {
              entry.fragment.destroy();
            }
            // Remount with new component
            const target = entry.target;
            if (target && newModule.default.mount) {
              const newFragment = newModule.default.mount(target, entry.props || {});
              entry.fragment = newFragment;
              entry.component = newModule.default;
            }
          }
        }
      }
    }
  });
}
"#,
        );
    }

    let source_map = Some(super::utils::generate_source_map(file_path, source, &code));

    Ok(TransformOutput {
        code,
        source_map,
        css_modules: None,
        is_css: false,
        extracted_css,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}

/// Convert parsed HTML nodes to Svelte DOM construction code.
///
/// Supports Svelte control-flow blocks that survive the HTML parser as text
/// nodes (the parser treats `{#if ...}`, `{:else}`, `{/if}`, `{#each ...}`,
/// `{/each}` as text between elements):
/// - `{#if cond}` ... `{:else}` ... `{/if}` → `if (cond) { ... } else { ... }`
/// - `{#each items as item}` ... `{/each}` → `items.forEach(item => { ... })`
///
/// Known limitations (unsupported Svelte features; see docs/LIMITATIONS.md):
/// - `$:` reactive statements (in `<script>`) — not yet compiled to a reactive
///   update function.
/// - `$store` auto-subscription syntax — not yet lowered to
///   `store.subscribe()`.
/// - `{#await}` / `{:then}` / `{:catch}` / `{/await}` blocks.
/// - `onMount` / `onDestroy` / other lifecycle hooks (passed through as-is).
/// - Nested control flow inside `{#if}`/`{#each}` (only one level deep is
///   rendered; deeper nesting is skipped with a placeholder).
fn nodes_to_svelte_render(nodes: &[HtmlNode], depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let mut code = String::new();

    if nodes.is_empty() {
        code.push_str(&format!(
            "{}root = document.createElement('div');\n",
            indent
        ));
        return code;
    }

    if nodes.len() == 1 {
        code.push_str(&node_to_svelte_dom(&nodes[0], "root", depth));
        return code;
    }

    code.push_str(&format!(
        "{}root = document.createDocumentFragment();\n",
        indent
    ));
    let mut i = 0;
    let mut child_idx = 0usize;
    while i < nodes.len() {
        let node = &nodes[i];
        // Detect Svelte control-flow markers (parsed as text nodes).
        if let HtmlNode::Text(t) = node {
            let trimmed = t.trim();
            if let Some(cond) = parse_svelte_if_open(trimmed) {
                let (end_idx, else_idx) = find_svelte_if_block(nodes, i);
                if let Some(end) = end_idx {
                    let true_nodes = &nodes[i + 1..else_idx.unwrap_or(end)];
                    let false_nodes = if let Some(ei) = else_idx {
                        &nodes[ei + 1..end]
                    } else {
                        &[]
                    };
                    let frag_var = format!("__svelte_if_{}_{}", depth, i);
                    code.push_str(&format!(
                        "{}const {} = document.createDocumentFragment();\n",
                        indent, frag_var
                    ));
                    code.push_str(&format!("{}if ({}) {{\n", indent, cond));
                    code.push_str(&render_svelte_nodes_into(
                        true_nodes,
                        &frag_var,
                        depth + 2,
                        &format!("{}_t", frag_var),
                    ));
                    if !false_nodes.is_empty() {
                        code.push_str(&format!("{}}} else {{\n", indent));
                        code.push_str(&render_svelte_nodes_into(
                            false_nodes,
                            &frag_var,
                            depth + 2,
                            &format!("{}_f", frag_var),
                        ));
                    }
                    code.push_str(&format!("{}}}\n", indent));
                    code.push_str(&format!("{}root.appendChild({});\n", indent, frag_var));
                    i = end + 1;
                    continue;
                }
                // No matching close: fall through and render as ordinary text.
            } else if let Some((list_expr, binding)) = parse_svelte_each_open(trimmed) {
                if let Some(end) = find_svelte_each_block(nodes, i) {
                    let each_nodes = &nodes[i + 1..end];
                    let frag_var = format!("__svelte_each_{}_{}", depth, i);
                    code.push_str(&format!(
                        "{}const {} = document.createDocumentFragment();\n",
                        indent, frag_var
                    ));
                    code.push_str(&format!(
                        "{}({} || []).forEach(({}) => {{\n",
                        indent, list_expr, binding
                    ));
                    code.push_str(&render_svelte_nodes_into(
                        each_nodes,
                        &frag_var,
                        depth + 2,
                        &format!("{}_n", frag_var),
                    ));
                    code.push_str(&format!("{}}});\n", indent));
                    code.push_str(&format!("{}root.appendChild({});\n", indent, frag_var));
                    i = end + 1;
                    continue;
                }
            } else if trimmed.starts_with("{:else")
                || trimmed.starts_with("{/if}")
                || trimmed.starts_with("{/each}")
            {
                // Stray marker (e.g. from a malformed block); skip it.
                i += 1;
                continue;
            }
        }
        let var = format!("child_{}", child_idx);
        code.push_str(&node_to_svelte_dom(node, &var, depth));
        code.push_str(&format!("{}root.appendChild({});\n", indent, var));
        child_idx += 1;
        i += 1;
    }

    code
}

/// Parse a `{#if cond}` marker, returning the condition expression.
fn parse_svelte_if_open(text: &str) -> Option<String> {
    let t = text.trim();
    if t.starts_with("{#if ") && t.ends_with('}') {
        Some(t[5..t.len() - 1].trim().to_string())
    } else {
        None
    }
}

/// Parse a `{#each items as item}` marker, returning `(list_expr, binding)`.
fn parse_svelte_each_open(text: &str) -> Option<(String, String)> {
    let t = text.trim();
    if t.starts_with("{#each ") && t.ends_with('}') {
        let body = t[7..t.len() - 1].trim();
        if let Some(idx) = body.find(" as ") {
            let list = body[..idx].trim().to_string();
            let binding = body[idx + 4..].trim().to_string();
            return Some((list, binding));
        }
    }
    None
}

/// Find the extent of a `{#if}` block starting at `start`.
///
/// Returns `(end_idx, else_idx)` where `end_idx` is the index of the matching
/// `{/if}` marker and `else_idx` is the index of the `{:else}` marker (if any).
fn find_svelte_if_block(nodes: &[HtmlNode], start: usize) -> (Option<usize>, Option<usize>) {
    let mut depth: isize = 1;
    let mut j = start + 1;
    let mut else_idx = None;
    let mut end_idx = None;
    while j < nodes.len() {
        if let HtmlNode::Text(t) = &nodes[j] {
            let trimmed = t.trim();
            if trimmed.starts_with("{#if") {
                depth += 1;
            } else if trimmed.starts_with("{:else if") || trimmed.starts_with("{:else}") {
                // Known limitation: `{:else if}` chains are not expanded; the first
                // else-family marker at depth 1 acts as the else boundary.
                if depth == 1 && else_idx.is_none() {
                    else_idx = Some(j);
                }
            } else if trimmed.starts_with("{/if}") {
                depth -= 1;
                if depth == 0 {
                    end_idx = Some(j);
                    break;
                }
            }
        }
        j += 1;
    }
    (end_idx, else_idx)
}

/// Find the matching `{/each}` for a `{#each}` block starting at `start`.
fn find_svelte_each_block(nodes: &[HtmlNode], start: usize) -> Option<usize> {
    let mut depth: isize = 1;
    let mut j = start + 1;
    while j < nodes.len() {
        if let HtmlNode::Text(t) = &nodes[j] {
            let trimmed = t.trim();
            if trimmed.starts_with("{#each") {
                depth += 1;
            } else if trimmed.starts_with("{/each}") {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
        }
        j += 1;
    }
    None
}

/// Render a slice of nodes into a container fragment variable.
///
/// Each node is created with a uniquely-prefixed variable name and appended to
/// `container`. Svelte control-flow markers are skipped here (only one level
/// of nesting is supported by `nodes_to_svelte_render`; deeper nesting is a
/// documented limitation).
fn render_svelte_nodes_into(
    nodes: &[HtmlNode],
    container: &str,
    depth: usize,
    prefix: &str,
) -> String {
    let indent = "  ".repeat(depth);
    let mut code = String::new();
    let mut idx = 0usize;
    for node in nodes.iter() {
        if let HtmlNode::Text(t) = node {
            let trimmed = t.trim();
            if trimmed.starts_with("{#") || trimmed.starts_with("{:") || trimmed.starts_with("{/") {
                // Known limitation: nested control flow inside {#if}/{#each} is skipped.
                continue;
            }
        }
        let var = format!("{}_{}", prefix, idx);
        code.push_str(&node_to_svelte_dom(node, &var, depth));
        code.push_str(&format!("{}{}.appendChild({});\n", indent, container, var));
        idx += 1;
    }
    code
}

/// Convert a single HTML node to Svelte DOM creation code
fn node_to_svelte_dom(node: &HtmlNode, var: &str, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    match node {
        HtmlNode::Text(text) => {
            if text.contains("{{") {
                let cleaned = text.replace("{{", "").replace("}}", "");
                let expr = cleaned.trim();
                format!(
                    "{}const {} = document.createTextNode(String({}));\n",
                    indent, var, expr
                )
            } else {
                format!(
                    "{}const {} = document.createTextNode('{}');\n",
                    indent,
                    var,
                    escape_js_string(text)
                )
            }
        }
        HtmlNode::Element {
            tag,
            attrs,
            children,
            ..
        } => {
            let mut code = String::new();
            code.push_str(&format!(
                "{}const {} = document.createElement('{}');\n",
                indent, var, tag
            ));

            for (name, value) in attrs {
                if let Some(event) = name.strip_prefix("on:") {
                    code.push_str(&format!(
                        "{}{}.addEventListener('{}', (e) => {{ {} }});\n",
                        indent, var, event, value
                    ));
                } else if let Some(prop) = name.strip_prefix("bind:") {
                    code.push_str(&format!(
                        "{}{}.{} = {};\n{}{}.addEventListener('input', (e) => {{ {} = e.target.{} }});\n",
                        indent, var, prop, value, indent, var, value, prop
                    ));
                } else if name.starts_with("{") && name.ends_with("}") {
                    let expr = name.trim_start_matches('{').trim_end_matches('}').trim();
                    code.push_str(&format!(
                        "{}{}.setAttribute('data-svelte-expr', '{}');\n",
                        indent,
                        var,
                        escape_js_string(expr)
                    ));
                } else if name == "class" {
                    code.push_str(&format!(
                        "{}{}.className = '{}';\n",
                        indent,
                        var,
                        escape_js_string(value)
                    ));
                } else if name == "style" {
                    code.push_str(&format!(
                        "{}{}.setAttribute('style', '{}');\n",
                        indent,
                        var,
                        escape_js_string(value)
                    ));
                } else {
                    code.push_str(&format!(
                        "{}{}.setAttribute('{}', '{}');\n",
                        indent,
                        var,
                        name,
                        escape_js_string(value)
                    ));
                }
            }

            for (i, child) in children.iter().enumerate() {
                let child_var = format!("{}_child_{}", var, i);
                code.push_str(&node_to_svelte_dom(child, &child_var, depth + 1));
                code.push_str(&format!("{}{}.appendChild({});\n", indent, var, child_var));
            }

            code
        }
    }
}

/// Extract Svelte markup (everything outside <script> and <style>)
fn extract_svelte_markup(source: &str) -> String {
    let mut markup = source.to_string();

    if let Some(start) = markup.find("<script")
        && let Some(end) = markup.find("</script>")
    {
        let end_full = end + "</script>".len();
        let before = &markup[..start];
        let after = &markup[end_full..];
        markup = format!("{}{}", before, after);
    }

    if let Some(start) = markup.find("<style")
        && let Some(end) = markup.find("</style>")
    {
        let end_full = end + "</style>".len();
        let before = &markup[..start];
        let after = &markup[end_full..];
        markup = format!("{}{}", before, after);
    }

    markup.trim().to_string()
}

// ─── Astro Parser ────────────────────────────────────────────────────

/// Transform an Astro component (.astro)
/// Extracts frontmatter (---), template, and styles
/// Produces a JS module with a render function
pub(super) fn transform_astro(
    source: &str,
    file_path: &str,
    is_production: bool,
) -> Result<TransformOutput> {
    let mut code = String::new();
    let mut extracted_css = None;

    let frontmatter = extract_astro_frontmatter(source);
    let template = extract_astro_template(source);

    if let Some(style_content) = extract_sfc_block(source, "style") {
        extracted_css = Some(style_content);
    }

    code.push_str("// Astro component — compiled by Pledge\n");

    if let Some(fm) = &frontmatter {
        let allocator = Allocator::default();
        let ParserReturn {
            mut program,
            panicked,
            ..
        } = Parser::new(&allocator, fm, SourceType::ts()).parse();
        if !panicked {
            let mut options = TransformOptions::default();
            options.typescript.only_remove_type_imports = false;
            let semantic = oxc::semantic::SemanticBuilder::new()
                .with_check_syntax_error(false)
                .build(&program);
            let transformer = Transformer::new(&allocator, Path::new(file_path), &options);
            let scoping = semantic.semantic.into_scoping();
            let _ = transformer.build_with_scoping(scoping, &mut program);
            let result = Codegen::new().build(&program);
            code.push_str(&result.code);
        } else {
            code.push_str(fm);
        }
        code.push('\n');
    }

    let escaped_template = template.replace('\n', "\\n").replace('"', "\\\"");
    code.push_str(&format!(
        r#"
// Astro render function
export async function render(props) {{
  return `{}`;
}}

export default {{
  render,
}};
"#,
        escaped_template
    ));

    if !is_production {
        code.push_str("\n// Astro HMR\nif (import.meta.hot) {\n  import.meta.hot.accept();\n}\n");
    }

    let source_map = Some(super::utils::generate_source_map(file_path, source, &code));

    Ok(TransformOutput {
        code,
        source_map,
        css_modules: None,
        is_css: false,
        extracted_css,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}

/// Extract Astro frontmatter (between --- markers)
fn extract_astro_frontmatter(source: &str) -> Option<String> {
    let first = source.find("---")?;
    let rest = &source[first + 3..];
    let second = rest.find("---")?;
    Some(rest[..second].trim().to_string())
}

/// Extract Astro template (everything after the last ---)
fn extract_astro_template(source: &str) -> String {
    if let Some(first) = source.find("---") {
        let rest = &source[first + 3..];
        if let Some(second) = rest.find("---") {
            let after = &rest[second + 3..];
            let mut template = after.to_string();
            if let Some(s_start) = template.find("<style")
                && let Some(s_end) = template.find("</style>")
            {
                let end_full = s_end + "</style>".len();
                template = format!("{}{}", &template[..s_start], &template[end_full..]);
            }
            return template.trim().to_string();
        }
    }
    source.trim().to_string()
}
