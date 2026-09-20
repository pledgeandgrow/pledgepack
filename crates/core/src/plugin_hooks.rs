//! Bridge between the build engine and per-module plugin hooks.
//!
//! `pledgepack-js-plugin-host` depends on `pledgepack-core`, so the engine
//! cannot call the host directly. Instead the engine talks to the
//! [`PluginHooks`] trait defined here; the embedder (the CLI) implements it
//! with a real plugin host and passes it to
//! [`BuildEngine::build_with_hooks`](crate::engine::BuildEngine::build_with_hooks)
//! and the `emit_*_with_hooks` methods.
//!
//! The trait is deliberately *per plugin*: it exposes each plugin's hooks
//! individually and leaves the Rollup/Vite ordering semantics to
//! [`HookRunner`], so they are implemented (and tested) exactly once:
//!
//! | hook                 | semantics                                           |
//! |----------------------|-----------------------------------------------------|
//! | `resolveId`          | plugin order, first non-null result wins            |
//! | `load`               | plugin order, first non-null result wins            |
//! | `transform`          | chained in plugin order (each sees the last output) |
//! | `renderChunk`        | chained in plugin order, before content hashing     |
//! | `transformIndexHtml` | chained in plugin order, before the HTML is written |
//!
//! Any error returned by a plugin aborts the build; [`HookRunner`] adds the
//! plugin name, hook name and file to the message.
//!
//! The trait does not require `Send`/`Sync`: plugin runtimes such as QuickJS
//! are single-threaded, so every hook is invoked from the thread that drives
//! the build, never from the rayon transform pool.

use anyhow::{Result, anyhow};

/// The per-module hooks the build engine can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildHook {
    ResolveId,
    Load,
    Transform,
    RenderChunk,
    TransformIndexHtml,
}

impl BuildHook {
    /// The hook's name as plugin authors write it.
    pub fn name(self) -> &'static str {
        match self {
            Self::ResolveId => "resolveId",
            Self::Load => "load",
            Self::Transform => "transform",
            Self::RenderChunk => "renderChunk",
            Self::TransformIndexHtml => "transformIndexHtml",
        }
    }
}

/// Result of a plugin `resolveId` hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedId {
    /// Resolved id: an absolute file path, a project-relative path, or an
    /// arbitrary virtual id that a `load` hook must then provide code for.
    pub id: String,
    /// The import is external — it is left as-is and not bundled.
    pub external: bool,
}

/// Result of a plugin `load` / `transform` / `renderChunk` hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeResult {
    pub code: String,
    pub map: Option<String>,
}

/// A tag a `transformIndexHtml` hook asked to inject.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HtmlTagSpec {
    pub tag: String,
    pub attrs: Vec<(String, String)>,
    pub children: Option<String>,
    /// `head`, `body`, `head-prepend` or `body-prepend` (default `head`).
    pub inject_to: Option<String>,
}

/// Result of a plugin `transformIndexHtml` hook.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HtmlResult {
    /// Replacement HTML, if the plugin returned a string / `{ html }`.
    pub html: Option<String>,
    /// Tags to inject.
    pub tags: Vec<HtmlTagSpec>,
}

/// What the build engine needs from a plugin host. Implemented by the
/// embedder (the CLI implements it with `JsPluginHost`).
///
/// Every hook returns `Ok(None)` for "no opinion" (the plugin lacks the hook
/// or returned `null`/`undefined`) and `Err` when the plugin threw — errors
/// abort the build. `plugin` is an index below [`plugin_count`](Self::plugin_count).
pub trait PluginHooks {
    /// Number of loaded plugins, in execution order.
    fn plugin_count(&self) -> usize;
    /// Display name of a plugin (used in error messages).
    fn plugin_name(&self, plugin: usize) -> String;
    /// Whether a plugin defines the given hook.
    fn supports(&self, plugin: usize, hook: BuildHook) -> bool;

    /// A stable fingerprint of everything that determines what this plugin's
    /// `transform` hook returns for a given `(code, id)`: its identity, its
    /// version, its source and its configuration.
    ///
    /// The engine uses it (together with the module id and input source) as
    /// the key of its plugin-transform cache. Return `None` (the default)
    /// when the plugin's output cannot be described by a fingerprint - it
    /// reads other files, the clock, environment, etc. - and its results are
    /// then never cached. A wrong `Some` produces stale builds, so hosts must
    /// only return one for plugins they know to be pure.
    fn transform_fingerprint(&self, _plugin: usize) -> Option<String> {
        None
    }

    fn resolve_id(
        &self,
        plugin: usize,
        source: &str,
        importer: Option<&str>,
    ) -> Result<Option<ResolvedId>>;
    fn load(&self, plugin: usize, id: &str) -> Result<Option<CodeResult>>;
    fn transform(&self, plugin: usize, code: &str, id: &str) -> Result<Option<CodeResult>>;
    fn render_chunk(
        &self,
        plugin: usize,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Result<Option<CodeResult>>;
    fn transform_index_html(
        &self,
        plugin: usize,
        html: &str,
        filename: &str,
    ) -> Result<Option<HtmlResult>>;
}

/// Applies the Rollup/Vite ordering semantics on top of a [`PluginHooks`].
pub struct HookRunner<'a> {
    hooks: &'a dyn PluginHooks,
}

impl<'a> HookRunner<'a> {
    pub fn new(hooks: &'a dyn PluginHooks) -> Self {
        Self { hooks }
    }

    fn fail(
        &self,
        plugin: usize,
        hook: BuildHook,
        subject: &str,
        e: anyhow::Error,
    ) -> anyhow::Error {
        anyhow!(
            "plugin '{}' failed in `{}` hook for {}: {:#}",
            self.hooks.plugin_name(plugin),
            hook.name(),
            subject,
            e
        )
    }

    /// `resolveId`: first plugin returning non-null wins.
    pub fn resolve_id(&self, source: &str, importer: Option<&str>) -> Result<Option<ResolvedId>> {
        for i in 0..self.hooks.plugin_count() {
            if !self.hooks.supports(i, BuildHook::ResolveId) {
                continue;
            }
            match self.hooks.resolve_id(i, source, importer) {
                Ok(Some(r)) => return Ok(Some(r)),
                Ok(None) => {}
                Err(e) => {
                    let subject = match importer {
                        Some(imp) => format!("import '{source}' in {imp}"),
                        None => format!("entry '{source}'"),
                    };
                    return Err(self.fail(i, BuildHook::ResolveId, &subject, e));
                }
            }
        }
        Ok(None)
    }

    /// `load`: first plugin returning non-null wins.
    pub fn load(&self, id: &str) -> Result<Option<CodeResult>> {
        for i in 0..self.hooks.plugin_count() {
            if !self.hooks.supports(i, BuildHook::Load) {
                continue;
            }
            match self.hooks.load(i, id) {
                Ok(Some(r)) => return Ok(Some(r)),
                Ok(None) => {}
                Err(e) => return Err(self.fail(i, BuildHook::Load, id, e)),
            }
        }
        Ok(None)
    }

    /// Fingerprint of the whole `transform` chain: the ordered fingerprints of
    /// every plugin that defines `transform`. `None` if any of them is
    /// uncacheable (see [`PluginHooks::transform_fingerprint`]) - one impure
    /// plugin makes the composed output impure. With no transform plugins the
    /// (empty) fingerprint is trivially stable.
    pub fn transform_fingerprint(&self) -> Option<String> {
        let mut out = String::new();
        for i in 0..self.hooks.plugin_count() {
            if !self.hooks.supports(i, BuildHook::Transform) {
                continue;
            }
            let fp = self.hooks.transform_fingerprint(i)?;
            out.push_str(&format!("{}:{}:{};", i, self.hooks.plugin_name(i), fp));
        }
        Some(out)
    }

    /// `transform`: chained in plugin order. Returns `None` when no plugin
    /// changed the code.
    pub fn transform(&self, code: &str, id: &str) -> Result<Option<CodeResult>> {
        self.chain(BuildHook::Transform, code, id, |i, current| {
            self.hooks.transform(i, current, id)
        })
    }

    /// `renderChunk`: chained in plugin order. Returns `None` when no plugin
    /// changed the code.
    pub fn render_chunk(
        &self,
        code: &str,
        filename: &str,
        chunk_type: &str,
    ) -> Result<Option<CodeResult>> {
        self.chain(BuildHook::RenderChunk, code, filename, |i, current| {
            self.hooks.render_chunk(i, current, filename, chunk_type)
        })
    }

    fn chain(
        &self,
        hook: BuildHook,
        code: &str,
        subject: &str,
        call: impl Fn(usize, &str) -> Result<Option<CodeResult>>,
    ) -> Result<Option<CodeResult>> {
        let mut current: Option<CodeResult> = None;
        for i in 0..self.hooks.plugin_count() {
            if !self.hooks.supports(i, hook) {
                continue;
            }
            let input = current.as_ref().map_or(code, |c| c.code.as_str());
            match call(i, input) {
                Ok(Some(mut r)) => {
                    // Each plugin's map is relative to ITS input, which for
                    // every plugin after the first change is the previous
                    // plugin's output. Compose so the final map traces all
                    // the way back to the original source; if any link in
                    // the chain has no map (or cannot be composed) the
                    // lineage is broken and no map is reported rather than a
                    // wrong one.
                    r.map = match (&current, r.map.take()) {
                        (None, map) => map,
                        (Some(prev), Some(map)) => prev.map.as_deref().and_then(|prev_map| {
                            crate::sourcemap_compose::compose_source_maps(&map, prev_map)
                        }),
                        (Some(_), None) => None,
                    };
                    current = Some(r);
                }
                Ok(None) => {}
                Err(e) => return Err(self.fail(i, hook, subject, e)),
            }
        }
        Ok(current)
    }

    /// `transformIndexHtml`: chained in plugin order; injected tags are
    /// applied after each plugin so later plugins see them.
    pub fn transform_index_html(&self, html: &str, filename: &str) -> Result<String> {
        let mut current = html.to_string();
        for i in 0..self.hooks.plugin_count() {
            if !self.hooks.supports(i, BuildHook::TransformIndexHtml) {
                continue;
            }
            match self.hooks.transform_index_html(i, &current, filename) {
                Ok(Some(r)) => {
                    if let Some(h) = r.html {
                        current = h;
                    }
                    current = inject_html_tags(&current, &r.tags);
                }
                Ok(None) => {}
                Err(e) => return Err(self.fail(i, BuildHook::TransformIndexHtml, filename, e)),
            }
        }
        Ok(current)
    }
}

const VOID_TAGS: &[&str] = &["meta", "link", "base", "img", "input", "br", "hr"];

fn render_tag(t: &HtmlTagSpec) -> String {
    let mut s = format!("<{}", t.tag);
    for (k, v) in &t.attrs {
        if v.is_empty() {
            s.push_str(&format!(" {k}"));
        } else {
            s.push_str(&format!(" {k}=\"{}\"", v.replace('"', "&quot;")));
        }
    }
    s.push('>');
    if !VOID_TAGS.contains(&t.tag.as_str()) {
        s.push_str(t.children.as_deref().unwrap_or(""));
        s.push_str(&format!("</{}>", t.tag));
    }
    s
}

/// Inject `tags` into `html` at their requested locations.
pub fn inject_html_tags(html: &str, tags: &[HtmlTagSpec]) -> String {
    let mut out = html.to_string();
    for t in tags {
        let rendered = format!("    {}\n", render_tag(t));
        let target = t.inject_to.as_deref().unwrap_or("head");
        let (close, open) = match target {
            "body" => ("</body>", None),
            "body-prepend" => ("", Some("<body")),
            "head-prepend" => ("", Some("<head")),
            _ => ("</head>", None),
        };
        if let Some(open_tag) = open {
            // Insert right after the opening tag's `>`.
            if let Some(start) = out.find(open_tag)
                && let Some(gt) = out[start..].find('>')
            {
                out.insert_str(start + gt + 1, &format!("\n{}", rendered.trim_end()));
                continue;
            }
        } else if let Some(pos) = out.rfind(close) {
            out.insert_str(pos, &rendered);
            continue;
        }
        // No anchor found: append so the tag is never silently lost.
        out.push_str(&rendered);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scripted fake host recording every call.
    #[derive(Default)]
    struct Fake {
        names: Vec<&'static str>,
        calls: RefCell<Vec<String>>,
        resolve: Vec<Option<ResolvedId>>,
        load: Vec<Option<&'static str>>,
        /// (find, replace) per plugin for transform/renderChunk
        edit: Vec<Option<(&'static str, &'static str)>>,
        fail_hook: Option<(usize, BuildHook)>,
        html: Vec<Option<HtmlResult>>,
    }

    impl Fake {
        fn err_if(&self, i: usize, h: BuildHook) -> Result<()> {
            if self.fail_hook == Some((i, h)) {
                anyhow::bail!("boom");
            }
            Ok(())
        }
        fn apply(&self, i: usize, code: &str) -> Option<CodeResult> {
            self.edit
                .get(i)
                .copied()
                .flatten()
                .map(|(f, r)| CodeResult {
                    code: code.replace(f, r),
                    map: None,
                })
        }
    }

    impl PluginHooks for Fake {
        fn plugin_count(&self) -> usize {
            self.names.len()
        }
        fn plugin_name(&self, p: usize) -> String {
            self.names[p].to_string()
        }
        fn supports(&self, _p: usize, _h: BuildHook) -> bool {
            true
        }
        fn resolve_id(&self, p: usize, s: &str, _i: Option<&str>) -> Result<Option<ResolvedId>> {
            self.calls.borrow_mut().push(format!("resolve:{p}:{s}"));
            self.err_if(p, BuildHook::ResolveId)?;
            Ok(self.resolve.get(p).cloned().flatten())
        }
        fn load(&self, p: usize, id: &str) -> Result<Option<CodeResult>> {
            self.calls.borrow_mut().push(format!("load:{p}:{id}"));
            self.err_if(p, BuildHook::Load)?;
            Ok(self.load.get(p).copied().flatten().map(|c| CodeResult {
                code: c.to_string(),
                map: None,
            }))
        }
        fn transform(&self, p: usize, code: &str, id: &str) -> Result<Option<CodeResult>> {
            self.calls.borrow_mut().push(format!("transform:{p}:{id}"));
            self.err_if(p, BuildHook::Transform)?;
            Ok(self.apply(p, code))
        }
        fn render_chunk(
            &self,
            p: usize,
            code: &str,
            f: &str,
            _t: &str,
        ) -> Result<Option<CodeResult>> {
            self.calls.borrow_mut().push(format!("render:{p}:{f}"));
            self.err_if(p, BuildHook::RenderChunk)?;
            Ok(self.apply(p, code))
        }
        fn transform_index_html(&self, p: usize, _h: &str, _f: &str) -> Result<Option<HtmlResult>> {
            self.err_if(p, BuildHook::TransformIndexHtml)?;
            Ok(self.html.get(p).cloned().flatten())
        }
    }

    fn rid(id: &str) -> Option<ResolvedId> {
        Some(ResolvedId {
            id: id.into(),
            external: false,
        })
    }

    #[test]
    fn resolve_id_first_non_null_wins_and_stops() {
        let fake = Fake {
            names: vec!["a", "b", "c"],
            resolve: vec![None, rid("B"), rid("C")],
            ..Default::default()
        };
        let r = HookRunner::new(&fake);
        assert_eq!(r.resolve_id("x", None).unwrap(), rid("B"));
        // plugin c was never consulted
        assert_eq!(*fake.calls.borrow(), vec!["resolve:0:x", "resolve:1:x"]);
    }

    #[test]
    fn load_first_non_null_wins() {
        let fake = Fake {
            names: vec!["a", "b", "c"],
            load: vec![None, Some("from-b"), Some("from-c")],
            ..Default::default()
        };
        let got = HookRunner::new(&fake).load("id").unwrap().unwrap();
        assert_eq!(got.code, "from-b");
        assert_eq!(fake.calls.borrow().len(), 2);
    }

    #[test]
    fn transform_chains_in_plugin_order() {
        let fake = Fake {
            names: vec!["a", "b", "c"],
            // a: X->Y, b: no opinion, c: Y->Z — only order-respecting chaining yields Z
            edit: vec![Some(("X", "Y")), None, Some(("Y", "Z"))],
            ..Default::default()
        };
        let got = HookRunner::new(&fake)
            .transform("X", "f.js")
            .unwrap()
            .unwrap();
        assert_eq!(got.code, "Z");
        // Reverse order would leave "Y": prove order matters.
        let fake = Fake {
            names: vec!["c", "a"],
            edit: vec![Some(("Y", "Z")), Some(("X", "Y"))],
            ..Default::default()
        };
        let got = HookRunner::new(&fake)
            .transform("X", "f.js")
            .unwrap()
            .unwrap();
        assert_eq!(got.code, "Y");
    }

    #[test]
    fn transform_returns_none_when_nobody_changes_anything() {
        let fake = Fake {
            names: vec!["a"],
            ..Default::default()
        };
        assert!(
            HookRunner::new(&fake)
                .transform("X", "f.js")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn render_chunk_chains() {
        let fake = Fake {
            names: vec!["a", "b"],
            edit: vec![Some(("a", "b")), Some(("b", "c"))],
            ..Default::default()
        };
        let got = HookRunner::new(&fake)
            .render_chunk("a", "entry.js", "entry")
            .unwrap()
            .unwrap();
        assert_eq!(got.code, "c");
    }

    #[test]
    fn errors_name_the_plugin_hook_and_file() {
        let fake = Fake {
            names: vec!["ok", "bad-plugin"],
            fail_hook: Some((1, BuildHook::Transform)),
            ..Default::default()
        };
        let err = HookRunner::new(&fake)
            .transform("X", "/src/app.ts")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bad-plugin"), "{err}");
        assert!(err.contains("transform"), "{err}");
        assert!(err.contains("/src/app.ts"), "{err}");
        assert!(err.contains("boom"), "{err}");

        let fake = Fake {
            names: vec!["p"],
            fail_hook: Some((0, BuildHook::ResolveId)),
            ..Default::default()
        };
        let err = HookRunner::new(&fake)
            .resolve_id("./x", Some("/src/a.ts"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'p'") && err.contains("./x") && err.contains("/src/a.ts"));
    }

    #[test]
    fn html_hook_chains_and_injects_tags() {
        let fake = Fake {
            names: vec!["a", "b"],
            html: vec![
                Some(HtmlResult {
                    html: Some("<html><head></head><body></body></html>".into()),
                    tags: vec![HtmlTagSpec {
                        tag: "meta".into(),
                        attrs: vec![("name".into(), "x".into())],
                        ..Default::default()
                    }],
                }),
                Some(HtmlResult {
                    html: None,
                    tags: vec![HtmlTagSpec {
                        tag: "script".into(),
                        children: Some("1".into()),
                        inject_to: Some("body".into()),
                        ..Default::default()
                    }],
                }),
            ],
            ..Default::default()
        };
        let out = HookRunner::new(&fake)
            .transform_index_html("<x>", "index.html")
            .unwrap();
        assert!(out.contains("<meta name=\"x\">\n</head>"), "{out}");
        assert!(out.contains("<script>1</script>\n</body>"), "{out}");
    }

    /// Plugins that each rewrite the code and return a map relative to
    /// THEIR input, to check the chain composes maps.
    struct MapPlugins {
        /// (map returned per plugin: `None` = code changed but no map)
        maps: Vec<Option<&'static str>>,
        fingerprints: Vec<Option<&'static str>>,
    }
    impl PluginHooks for MapPlugins {
        fn plugin_count(&self) -> usize {
            self.maps.len()
        }
        fn plugin_name(&self, i: usize) -> String {
            format!("p{i}")
        }
        fn supports(&self, _: usize, hook: BuildHook) -> bool {
            hook == BuildHook::Transform
        }
        fn transform_fingerprint(&self, i: usize) -> Option<String> {
            self.fingerprints[i].map(String::from)
        }
        fn resolve_id(&self, _: usize, _: &str, _: Option<&str>) -> Result<Option<ResolvedId>> {
            Ok(None)
        }
        fn load(&self, _: usize, _: &str) -> Result<Option<CodeResult>> {
            Ok(None)
        }
        fn transform(&self, i: usize, code: &str, _: &str) -> Result<Option<CodeResult>> {
            Ok(Some(CodeResult {
                code: format!("{code}+{i}"),
                map: self.maps[i].map(String::from),
            }))
        }
        fn render_chunk(&self, _: usize, _: &str, _: &str, _: &str) -> Result<Option<CodeResult>> {
            Ok(None)
        }
        fn transform_index_html(&self, _: usize, _: &str, _: &str) -> Result<Option<HtmlResult>> {
            Ok(None)
        }
    }

    // "AAAA" = (gen col 0) -> (source 0, line 0, col 0)
    // "AAAC" = (gen col 0) -> (source 0, line 0, col 1)
    const MAP_A: &str = r#"{"version":3,"sources":["b.ts"],"names":[],"mappings":"AAAC"}"#;
    const MAP_B: &str = r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"AAAA"}"#;

    #[test]
    fn transform_chain_composes_maps_back_to_the_original_source() {
        // Plugin 0: out0 -> original (a.ts col 0). Plugin 1 maps out1 -> out0 (col 1);
        // composed, out1 col 0 traces through inner col 0 (segment start <= 1) to a.ts.
        let hosts = MapPlugins {
            maps: vec![Some(MAP_B), Some(MAP_A)],
            fingerprints: vec![None, None],
        };
        let r = HookRunner::new(&hosts)
            .transform("x", "id")
            .unwrap()
            .unwrap();
        assert_eq!(r.code, "x+0+1");
        let map: serde_json::Value = serde_json::from_str(r.map.as_deref().unwrap()).unwrap();
        assert_eq!(map["sources"], serde_json::json!(["a.ts"]), "{map}");
        assert_eq!(map["mappings"], "AAAA", "{map}");
    }

    #[test]
    fn transform_chain_drops_the_map_when_a_link_has_none() {
        // Plugin 0 changed the code without a map -> plugin 1's map only reaches
        // plugin 0's output, which is not the original: must not be reported.
        let hosts = MapPlugins {
            maps: vec![None, Some(MAP_A)],
            fingerprints: vec![None, None],
        };
        let r = HookRunner::new(&hosts)
            .transform("x", "id")
            .unwrap()
            .unwrap();
        assert!(r.map.is_none(), "{:?}", r.map);
    }

    #[test]
    fn transform_fingerprint_requires_every_transform_plugin_to_have_one() {
        let all = MapPlugins {
            maps: vec![None, None],
            fingerprints: vec![Some("a"), Some("b")],
        };
        let fp = HookRunner::new(&all).transform_fingerprint().unwrap();
        assert!(fp.contains("a") && fp.contains("b") && fp.contains("p0") && fp.contains("p1"));

        let one_impure = MapPlugins {
            maps: vec![None, None],
            fingerprints: vec![Some("a"), None],
        };
        assert!(
            HookRunner::new(&one_impure)
                .transform_fingerprint()
                .is_none()
        );
    }
}
