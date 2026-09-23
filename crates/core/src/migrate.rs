// Config migration — converts Vite, webpack, CRA, and Next.js configs to pledge.config.ts
//
// The source config is parsed with Oxc (see `js_config`) and folded into a
// plain JSON value; nothing is executed and there is no hand-rolled brace
// scanner. The mapped settings are collected into a Pledgepack config JSON
// object which `pledge migrate` and `pledge init` both render.

use crate::js_config::{self, as_path_string, call_name, expr_head, is_dynamic};
use anyhow::Result;
use serde_json::{Map, Value, json};
use std::path::Path;

/// Migration result containing the new config content and a summary of what was migrated.
pub struct MigrationResult {
    /// Rendered `pledge.config.ts`.
    pub config_content: String,
    /// Output file name (always `pledge.config.ts`).
    pub config_path: String,
    /// The source file the settings were read from (e.g. `vite.config.ts`).
    pub source_file: String,
    /// Mapped Pledgepack settings (camelCase, same shape as `pledge.json`).
    pub config: Value,
    pub warnings: Vec<String>,
    pub migrated_fields: Vec<String>,
}

/// Detect and migrate config from existing build tools.
pub fn migrate_config(root: &Path) -> Result<MigrationResult> {
    for ext in &["ts", "js", "mjs", "cjs", "mts", "cts"] {
        let name = format!("vite.config.{}", ext);
        let path = root.join(&name);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            return Ok(migrate_vite_config(&content, &name, root));
        }
    }

    for ext in &["ts", "js", "cjs", "mjs"] {
        let name = format!("webpack.config.{}", ext);
        let path = root.join(&name);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            return Ok(migrate_webpack_config(&content, &name, root));
        }
    }

    let cra_path = root.join("config-overrides.js");
    if cra_path.exists() {
        let content = std::fs::read_to_string(&cra_path)?;
        return Ok(migrate_cra_config(&content, "config-overrides.js", root));
    }

    for ext in &["ts", "js", "mjs"] {
        let name = format!("next.config.{}", ext);
        let path = root.join(&name);
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            return Ok(migrate_next_config(&content, &name, root));
        }
    }

    // CRA without overrides: react-scripts in package.json.
    if package_json(root)
        .map(|p| dep_present(&p, "react-scripts"))
        .unwrap_or(false)
    {
        return Ok(migrate_cra_config("", "package.json", root));
    }

    anyhow::bail!(
        "No recognized config file found. Looking for: vite.config.{{ts,js,mjs}}, webpack.config.{{ts,js,cjs,mjs}}, config-overrides.js, next.config.{{ts,js,mjs}}, or react-scripts in package.json"
    )
}

// ─── Result accumulator ──────────────────────────────────────────────

struct Mapper<'a> {
    root: &'a Path,
    cfg: Map<String, Value>,
    warnings: Vec<String>,
    fields: Vec<String>,
}

impl<'a> Mapper<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            cfg: Map::new(),
            warnings: Vec::new(),
            fields: Vec::new(),
        }
    }

    /// Set `path` (e.g. `["devServer", "port"]`) in the output config.
    fn set(&mut self, path: &[&str], value: Value) {
        let mut cur = &mut self.cfg;
        for key in &path[..path.len() - 1] {
            let entry = cur
                .entry((*key).to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            cur = match entry {
                Value::Object(m) => m,
                _ => return,
            };
        }
        cur.insert(path[path.len() - 1].to_string(), value);
    }

    fn mapped(&mut self, from: &str, to: &str) {
        self.fields.push(format!("{} → {}", from, to));
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    /// Project-relative form of a path-ish string (`./src`, `/src`, absolute).
    fn rel(&self, s: &str) -> String {
        let s = s.replace('\\', "/");
        let p = Path::new(&s);
        if p.is_absolute()
            && let Ok(stripped) = p.strip_prefix(self.root)
        {
            return crate::normalize_path(stripped);
        }
        let trimmed = s.trim_start_matches("./");
        if let Some(rest) = trimmed.strip_prefix('/')
            && self.root.join(rest).exists()
        {
            return rest.to_string();
        }
        trimmed.to_string()
    }

    fn aliases(&mut self, src: &str, alias: &Value) {
        let mut out = Vec::new();
        match alias {
            Value::Object(map) if !is_dynamic(alias) => {
                for (k, v) in map {
                    match as_path_string(v) {
                        Some(to) => out.push((k.clone(), self.rel(&to))),
                        None => self.warn(format!(
                            "{} alias '{}' has a dynamic target that could not be evaluated",
                            src, k
                        )),
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    let find = item.get("find").and_then(|f| f.as_str());
                    let to = item.get("replacement").and_then(as_path_string);
                    match (find, to) {
                        (Some(f), Some(t)) => out.push((f.to_string(), self.rel(&t))),
                        _ => self.warn(format!(
                            "{} alias entry could not be evaluated (regex or dynamic `find`)",
                            src
                        )),
                    }
                }
            }
            _ => {}
        }
        if !out.is_empty() {
            self.set(
                &["resolveAlias"],
                Value::Array(
                    out.into_iter()
                        .map(|(from, to)| json!({ "from": from, "to": to }))
                        .collect(),
                ),
            );
            self.mapped(src, "resolveAlias");
        }
    }

    fn define(&mut self, src: &str, defs: &Value) {
        let Some(map) = defs.as_object() else { return };
        let mut out = Map::new();
        for (k, v) in map {
            match v {
                Value::String(s) => {
                    out.insert(k.clone(), Value::String(s.clone()));
                }
                Value::Bool(_) | Value::Number(_) | Value::Null => {
                    out.insert(k.clone(), Value::String(v.to_string()));
                }
                _ => match v
                    .get(js_config::EXPR_KEY)
                    .or_else(|| v.get(js_config::CALL_KEY))
                {
                    Some(_) => self.warn(format!(
                        "{} `{}` has a non-literal value and was not migrated",
                        src, k
                    )),
                    None => {
                        out.insert(k.clone(), Value::String(v.to_string()));
                    }
                },
            }
        }
        if !out.is_empty() {
            self.set(&["define"], Value::Object(out));
            self.mapped(src, "define");
        }
    }

    fn proxy(&mut self, src: &str, proxy: &Value) {
        let mut rules = Vec::new();
        match proxy {
            Value::Object(map) if !is_dynamic(proxy) => {
                for (key, val) in map {
                    let (target, has_rewrite, ws) = match val {
                        Value::String(t) => (Some(t.clone()), false, false),
                        Value::Object(o) => (
                            o.get("target").and_then(|t| t.as_str()).map(String::from),
                            o.contains_key("rewrite") || o.contains_key("pathRewrite"),
                            o.get("ws").and_then(|w| w.as_bool()).unwrap_or(false),
                        ),
                        _ => (None, false, false),
                    };
                    if let Some(t) = target {
                        let mut rule = json!({ "path": normalize_proxy_path(key), "target": t });
                        if has_rewrite {
                            rule["rewrite"] = json!(true);
                        }
                        if ws {
                            rule["ws"] = json!(true);
                        }
                        rules.push(rule);
                    }
                }
            }
            Value::Array(items) => {
                for it in items {
                    let target = it.get("target").and_then(|t| t.as_str());
                    let ctx = it.get("context");
                    let paths: Vec<String> = match ctx {
                        Some(Value::String(s)) => vec![s.clone()],
                        Some(Value::Array(a)) => a
                            .iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect(),
                        _ => vec![],
                    };
                    if let Some(t) = target {
                        for p in paths {
                            rules.push(json!({ "path": normalize_proxy_path(&p), "target": t }));
                        }
                    }
                }
            }
            _ => {}
        }
        if rules.is_empty() {
            self.warn(format!("{} proxy could not be evaluated statically", src));
        } else {
            self.set(&["proxy"], Value::Array(rules));
            self.mapped(src, "proxy");
        }
    }

    fn dev_server(&mut self, src: &str, server: &Value) {
        if let Some(port) = server.get("port").and_then(|p| p.as_u64())
            && let Ok(port) = u16::try_from(port)
        {
            self.set(&["devServer", "port"], json!(port));
            self.mapped(&format!("{}.port", src), "devServer.port");
        }
        match server.get("host") {
            Some(Value::String(h)) => {
                self.set(&["devServer", "host"], json!(h));
                self.mapped(&format!("{}.host", src), "devServer.host");
            }
            Some(Value::Bool(true)) => {
                self.set(&["devServer", "host"], json!("0.0.0.0"));
                self.mapped(&format!("{}.host", src), "devServer.host");
            }
            _ => {}
        }
        match server.get("open") {
            Some(Value::Bool(b)) => {
                self.set(&["devServer", "open"], json!(b));
                self.mapped(&format!("{}.open", src), "devServer.open");
            }
            Some(Value::String(_)) => {
                self.set(&["devServer", "open"], json!(true));
                self.mapped(&format!("{}.open", src), "devServer.open");
            }
            _ => {}
        }
        if let Some(Value::Bool(b)) = server.get("https") {
            self.set(&["devServer", "https"], json!(b));
            self.mapped(&format!("{}.https", src), "devServer.https");
        }
        if let Some(Value::Bool(b)) = server.get("hot") {
            self.set(&["devServer", "hmr"], json!(b));
            self.mapped(&format!("{}.hot", src), "devServer.hmr");
        }
        if let Some(proxy) = server.get("proxy") {
            self.proxy(&format!("{}.proxy", src), proxy);
        }
    }

    fn finish(
        mut self,
        source_file: &str,
        framework_default: Option<&str>,
        detection: &crate::detect::ProjectDetection,
    ) -> MigrationResult {
        // Entry + framework come from project detection (index.html script,
        // src/main.tsx, ...), never a hard-coded guess.
        if !self.cfg.contains_key("entry") {
            let entry = detection.entry_file.clone();
            if !self.root.join(&entry).exists() {
                self.warn(format!(
                    "Entry file '{}' was not found — set `entry` in pledge.config.ts",
                    entry
                ));
            }
            self.cfg.insert("entry".into(), json!([entry]));
        }
        if !self.cfg.contains_key("framework") {
            let fw = framework_default
                .map(String::from)
                .unwrap_or_else(|| detection.framework.pledge_framework().to_string());
            self.cfg.insert("framework".into(), json!(fw));
        }
        if self.fields.is_empty() {
            self.warn("No configurable fields were found in the source config. A default Pledgepack config was generated.");
        }
        let config = Value::Object(self.cfg);
        MigrationResult {
            config_content: render_config_ts(&config),
            config_path: "pledge.config.ts".to_string(),
            source_file: source_file.to_string(),
            config,
            warnings: self.warnings,
            migrated_fields: self.fields,
        }
    }
}

fn normalize_proxy_path(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{}", p)
    }
}

fn framework_for_plugin(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("react") {
        Some("react")
    } else if n.contains("vue") {
        Some("vue")
    } else if n.contains("svelte") {
        Some("svelte")
    } else if n.contains("solid") {
        Some("solid")
    } else {
        None
    }
}

// ─── Vite ────────────────────────────────────────────────────────────

fn migrate_vite_config(content: &str, file: &str, root: &Path) -> MigrationResult {
    let mut m = Mapper::new(root);
    let detection = crate::detect::detect_project(root);

    match js_config::eval_config_module(content, file) {
        Ok(obj) => map_vite(&mut m, &obj),
        Err(e) => m.warn(format!(
            "Could not statically read {}: {} — generated a default config",
            file, e
        )),
    }
    m.finish(file, None, &detection)
}

fn map_vite(m: &mut Mapper<'_>, obj: &Value) {
    if let Some(r) = obj.get("root").and_then(|v| v.as_str()) {
        m.warn(format!(
            "Vite `root` option '{}' — Pledgepack uses `root` from the --root flag instead",
            r
        ));
    }
    if let Some(b) = obj.get("base").and_then(|v| v.as_str()) {
        m.set(&["base"], json!(b));
        m.mapped("base", "base");
    }
    if let Some(dir) = obj.get("publicDir").and_then(as_path_string) {
        m.set(&["devServer", "publicDir"], json!(m.rel(&dir)));
        m.mapped("publicDir", "devServer.publicDir");
    }
    if let Some(server) = obj.get("server") {
        m.dev_server("server", server);
    }
    if let Some(alias) = obj.get("resolve").and_then(|r| r.get("alias")) {
        m.aliases("resolve.alias", alias);
    }
    if let Some(ext) = obj.get("resolve").and_then(|r| r.get("extensions"))
        && ext.is_array()
    {
        m.set(&["extensions"], ext.clone());
        m.mapped("resolve.extensions", "extensions");
    }
    if let Some(defs) = obj.get("define") {
        m.define("define", defs);
    }
    if let Some(prefix) = obj.get("envPrefix") {
        let list = match prefix {
            Value::String(s) => Some(json!([s])),
            Value::Array(_) => Some(prefix.clone()),
            _ => None,
        };
        if let Some(l) = list {
            m.set(&["envPrefix"], l);
            m.mapped("envPrefix", "envPrefix");
        }
    }
    if let Some(build) = obj.get("build") {
        if let Some(dir) = build.get("outDir").and_then(as_path_string) {
            m.set(&["outDir"], json!(m.rel(&dir)));
            m.mapped("build.outDir", "outDir");
        }
        match build.get("sourcemap") {
            Some(Value::Bool(b)) => {
                m.set(&["sourceMaps"], json!(b));
                m.mapped("build.sourcemap", "sourceMaps");
            }
            Some(Value::String(mode)) => {
                m.set(&["sourceMaps"], json!(true));
                let pledge_mode = match mode.as_str() {
                    "hidden" => Some("hidden"),
                    "inline" => Some("inline"),
                    _ => None,
                };
                if let Some(pm) = pledge_mode {
                    m.set(&["build", "sourceMapMode"], json!(pm));
                }
                m.mapped("build.sourcemap", "sourceMaps");
            }
            _ => {}
        }
        match build.get("target") {
            Some(Value::String(t)) => {
                m.set(&["build", "target"], json!(t));
                m.mapped("build.target", "build.target");
            }
            Some(Value::Array(a)) => {
                if let Some(t) = a.iter().find_map(|x| x.as_str()) {
                    m.set(&["build", "target"], json!(t));
                    m.mapped("build.target", "build.target");
                }
            }
            _ => {}
        }
        if let Some(n) = build.get("assetsInlineLimit").and_then(|v| v.as_u64()) {
            m.set(&["build", "assetsInlineLimit"], json!(n));
            m.mapped("build.assetsInlineLimit", "build.assetsInlineLimit");
        }
        if let Some(Value::Bool(b)) = build.get("modulePreload") {
            m.set(&["build", "modulePreload"], json!(b));
            m.mapped("build.modulePreload", "build.modulePreload");
        }
        if build.get("minify") == Some(&Value::Bool(false)) {
            m.warn("Vite `build.minify: false` — set `optimize.minify = false` in Pledgepack");
            m.set(&["optimize", "minify"], json!(false));
            m.mapped("build.minify", "optimize.minify");
        }
        if build.get("lib").is_some() {
            m.warn("Vite `build.lib` — configure Pledgepack's `library` option manually");
        }
    }
    if let Some(Value::Array(plugins)) = obj.get("plugins") {
        let mut names = Vec::new();
        for p in plugins {
            let name = call_name(p)
                .map(String::from)
                .or_else(|| expr_head(p))
                .or_else(|| p.as_str().map(String::from));
            if let Some(name) = name {
                names.push(name);
            }
        }
        for name in &names {
            match framework_for_plugin(name) {
                Some(fw) if !m.cfg.contains_key("framework") => {
                    m.set(&["framework"], json!(fw));
                    m.mapped(&format!("plugins: {}()", name), &format!("framework: '{}'", fw));
                }
                Some(_) => {}
                None => m.warn(format!(
                    "Vite plugin `{}()` has no automatic Pledgepack equivalent — add a Pledgepack plugin if you still need it",
                    name
                )),
            }
        }
    }
    for key in ["css", "worker", "ssr", "optimizeDeps", "test"] {
        if obj.get(key).is_some() {
            m.warn(format!(
                "Vite `{}` was not migrated (no direct equivalent)",
                key
            ));
        }
    }
}

// ─── webpack ─────────────────────────────────────────────────────────

fn migrate_webpack_config(content: &str, file: &str, root: &Path) -> MigrationResult {
    let mut m = Mapper::new(root);
    let detection = crate::detect::detect_project(root);

    match js_config::eval_config_module(content, file) {
        Ok(obj) => map_webpack(&mut m, &obj),
        Err(e) => m.warn(format!(
            "Could not statically read {}: {} — generated a default config",
            file, e
        )),
    }
    m.finish(file, None, &detection)
}

fn map_webpack(m: &mut Mapper<'_>, obj: &Value) {
    // entry: string | string[] | { name: string | string[] }
    let entry = match obj.get("entry") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => a.iter().find_map(as_path_string),
        Some(Value::Object(o)) if !is_dynamic(obj.get("entry").unwrap_or(&Value::Null)) => {
            o.values().find_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Array(a) => a.iter().find_map(as_path_string),
                Value::Object(inner) => inner.get("import").and_then(as_path_string),
                _ => None,
            })
        }
        _ => None,
    };
    if let Some(e) = entry {
        m.set(&["entry"], json!([m.rel(&e)]));
        m.mapped("entry", "entry");
    }

    if let Some(dir) = obj
        .get("output")
        .and_then(|o| o.get("path"))
        .and_then(as_path_string)
    {
        m.set(&["outDir"], json!(m.rel(&dir)));
        m.mapped("output.path", "outDir");
    }
    if let Some(public) = obj
        .get("output")
        .and_then(|o| o.get("publicPath"))
        .and_then(|p| p.as_str())
        && public != "auto"
    {
        m.set(&["base"], json!(public));
        m.mapped("output.publicPath", "base");
    }
    if let Some(ds) = obj.get("devServer") {
        m.dev_server("devServer", ds);
    }
    if let Some(alias) = obj.get("resolve").and_then(|r| r.get("alias")) {
        m.aliases("resolve.alias", alias);
    }
    if let Some(ext) = obj.get("resolve").and_then(|r| r.get("extensions"))
        && ext.is_array()
    {
        m.set(&["extensions"], ext.clone());
        m.mapped("resolve.extensions", "extensions");
    }
    match obj.get("devtool") {
        Some(Value::Bool(false)) => {
            m.set(&["sourceMaps"], json!(false));
            m.mapped("devtool", "sourceMaps");
        }
        Some(Value::String(_)) => {
            m.set(&["sourceMaps"], json!(true));
            m.mapped("devtool", "sourceMaps");
        }
        _ => {}
    }
    if let Some(Value::Array(plugins)) = obj.get("plugins") {
        for p in plugins {
            match call_name(p) {
                Some(n) if n.ends_with("DefinePlugin") => {
                    if let Some(defs) = p.get(js_config::ARGS_KEY).and_then(|a| a.get(0)) {
                        m.define("DefinePlugin", defs);
                    }
                }
                Some(n) if n.ends_with("HtmlWebpackPlugin") => {
                    if let Some(t) = p
                        .get(js_config::ARGS_KEY)
                        .and_then(|a| a.get(0))
                        .and_then(|o| o.get("template"))
                        .and_then(as_path_string)
                    {
                        m.set(&["htmlEntry"], json!(m.rel(&t)));
                        m.mapped("HtmlWebpackPlugin.template", "htmlEntry");
                    } else {
                        m.mapped("HtmlWebpackPlugin", "built-in HTML processing");
                    }
                }
                Some(n) if n.ends_with("MiniCssExtractPlugin") => {
                    m.mapped("MiniCssExtractPlugin", "built-in CSS extraction");
                }
                Some(n) => m.warn(format!(
                    "webpack plugin `{}` has no automatic Pledgepack equivalent",
                    n
                )),
                None => {}
            }
        }
    }
    let text = obj.to_string();
    if text.contains("sass-loader") || text.contains("scss") {
        m.warn("Sass/SCSS detected in webpack config — Pledgepack handles .scss/.sass natively");
    }
    if text.contains("postcss-loader") {
        m.warn("PostCSS detected in webpack config — Pledgepack has built-in PostCSS support");
    }
    if text.contains("babel-loader") || text.contains("ts-loader") {
        m.warn("babel-loader/ts-loader detected — Pledgepack transforms JS/TS with Oxc; custom Babel plugins are not carried over");
    }
    if let Some(mode) = obj.get("mode").and_then(|v| v.as_str()) {
        m.warn(format!(
            "webpack `mode: '{}'` — Pledgepack picks the mode from the command (`dev` / `build`)",
            mode
        ));
    }
}

// ─── CRA ─────────────────────────────────────────────────────────────

fn migrate_cra_config(content: &str, file: &str, root: &Path) -> MigrationResult {
    let mut m = Mapper::new(root);
    let detection = crate::detect::detect_project(root);

    if content.contains("rewireCss") || content.contains("sass") {
        m.warn("CSS overrides detected — Pledgepack handles Sass/SCSS natively");
    }
    if content.contains("rewireWebpack") || content.contains("webpack") {
        m.warn("Webpack overrides detected — review if equivalent Pledgepack config is needed");
    }

    // PORT from .env (CRA convention).
    if let Ok(env) = std::fs::read_to_string(root.join(".env")) {
        for line in env.lines() {
            if let Some(v) = line.trim().strip_prefix("PORT=")
                && let Ok(port) = v.trim().parse::<u16>()
            {
                m.set(&["devServer", "port"], json!(port));
                m.mapped(".env PORT", "devServer.port");
            }
        }
    }
    if let Some(pkg) = package_json(root) {
        if let Some(home) = pkg.get("homepage").and_then(|h| h.as_str()) {
            let base = home
                .split_once("://")
                .map(|(_, rest)| rest.find('/').map(|i| &rest[i..]).unwrap_or("/"))
                .unwrap_or(home);
            m.set(&["base"], json!(base));
            m.mapped("package.json homepage", "base");
        }
        if let Some(proxy) = pkg.get("proxy").and_then(|p| p.as_str()) {
            m.set(&["proxy"], json!([{ "path": "/api", "target": proxy }]));
            m.mapped("package.json proxy", "proxy (/api)");
        }
    }
    m.fields
        .push("entry (CRA uses src/index.{tsx,jsx,js})".to_string());
    m.finish(file, Some("react"), &detection)
}

// ─── Next.js ─────────────────────────────────────────────────────────

fn migrate_next_config(content: &str, file: &str, root: &Path) -> MigrationResult {
    let mut m = Mapper::new(root);
    let detection = crate::detect::detect_project(root);

    match js_config::eval_config_module(content, file) {
        Ok(obj) => {
            if let Some(b) = obj.get("basePath").and_then(|v| v.as_str()) {
                m.set(&["base"], json!(b));
                m.mapped("basePath", "base");
            }
            if let Some(d) = obj.get("distDir").and_then(|v| v.as_str()) {
                m.set(&["outDir"], json!(m.rel(d)));
                m.mapped("distDir", "outDir");
            }
            if let Some(Value::Object(env)) = obj.get("env") {
                let defs: Map<String, Value> = env
                    .iter()
                    .filter_map(|(k, v)| {
                        v.as_str().map(|s| {
                            (
                                format!("process.env.{}", k),
                                Value::String(format!("\"{}\"", s)),
                            )
                        })
                    })
                    .collect();
                if !defs.is_empty() {
                    m.set(&["define"], Value::Object(defs));
                    m.mapped("env", "define");
                }
            }
            for (key, msg) in [
                (
                    "images",
                    "Next.js `images` config — use Pledgepack's `image` config field instead",
                ),
                (
                    "rewrites",
                    "Next.js `rewrites` — configure as proxy rules in Pledgepack's devServer",
                ),
                (
                    "redirects",
                    "Next.js `redirects` — handle at your server/edge level",
                ),
                (
                    "headers",
                    "Next.js `headers` — set response headers at your server/edge level",
                ),
                (
                    "i18n",
                    "Next.js `i18n` — use Pledgepack's `i18n` config field instead",
                ),
            ] {
                if obj.get(key).is_some() {
                    m.warn(msg);
                }
            }
        }
        Err(e) => m.warn(format!(
            "Could not statically read {}: {} — generated a default config",
            file, e
        )),
    }
    if content.contains("appDir") {
        m.warn("Next.js App Router detected — use Pledgepack's Next.js adapter");
    }
    m.fields
        .push("framework (Next.js → Pledgepack Next adapter)".to_string());
    m.finish(file, Some("next"), &detection)
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn package_json(root: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).ok()?).ok()
}

fn dep_present(pkg: &Value, name: &str) -> bool {
    ["dependencies", "devDependencies"]
        .iter()
        .any(|k| pkg.get(k).and_then(|d| d.get(name)).is_some())
}

/// Overlay `overrides` (e.g. a CLI `--framework`) onto a config value.
pub fn set_top_level(config: &mut Value, key: &str, value: Value) {
    if let Some(o) = config.as_object_mut() {
        o.insert(key.to_string(), value);
    }
}

/// Render a Pledgepack config value as `pledge.config.ts`.
pub fn render_config_ts(config: &Value) -> String {
    let mut out =
        String::from("import { defineConfig } from 'pledgepack';\n\nexport default defineConfig(");
    write_js(config, 0, &mut out);
    out.push_str(");\n");
    out
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn js_string(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn write_js(v: &Value, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(&b.to_string()),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&js_string(s)),
        Value::Array(items) => {
            let simple = items
                .iter()
                .all(|i| !matches!(i, Value::Object(_) | Value::Array(_)));
            if items.is_empty() {
                out.push_str("[]");
            } else if simple {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_js(it, indent, out);
                }
                out.push(']');
            } else {
                out.push_str("[\n");
                for it in items {
                    out.push_str(&pad);
                    out.push_str("  ");
                    write_js(it, indent + 1, out);
                    out.push_str(",\n");
                }
                out.push_str(&pad);
                out.push(']');
            }
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (k, val) in map {
                out.push_str(&pad);
                out.push_str("  ");
                if is_ident(k) {
                    out.push_str(k);
                } else {
                    out.push_str(&js_string(k));
                }
                out.push_str(": ");
                write_js(val, indent + 1, out);
                out.push_str(",\n");
            }
            out.push_str(&pad);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        dir
    }

    const VITE: &str = r#"
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
export default defineConfig({
  plugins: [react()],
  server: { port: 5199, open: true, proxy: { '/api': { target: 'http://localhost:8080', rewrite: (p) => p } } },
  resolve: { alias: { '@': '/src' } },
  build: { outDir: 'build', sourcemap: true, target: 'es2020' },
  define: { __APP__: '"x"' },
})
"#;

    #[test]
    fn vite_settings_are_mapped_and_round_trip_through_the_config_loader() {
        let dir = project(&[
            ("vite.config.ts", VITE),
            ("src/main.tsx", "export {}"),
            (
                "index.html",
                "<html><body><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>",
            ),
        ]);
        let r = migrate_config(dir.path()).unwrap();
        assert_eq!(r.source_file, "vite.config.ts");
        let c = &r.config;
        assert_eq!(c["devServer"]["port"], json!(5199));
        assert_eq!(c["devServer"]["open"], json!(true));
        assert_eq!(c["outDir"], json!("build"));
        assert_eq!(c["sourceMaps"], json!(true));
        assert_eq!(c["build"]["target"], json!("es2020"));
        assert_eq!(c["define"]["__APP__"], json!("\"x\""));
        assert_eq!(c["framework"], json!("react"));
        assert_eq!(c["entry"], json!(["src/main.tsx"]));
        assert_eq!(c["resolveAlias"][0]["from"], json!("@"));
        assert_eq!(c["resolveAlias"][0]["to"], json!("src"));
        assert_eq!(c["proxy"][0]["path"], json!("/api"));
        assert_eq!(c["proxy"][0]["rewrite"], json!(true));

        // The rendered TS must load back into a real PledgeConfig.
        let cfg = crate::config::PledgeConfig::parse_ts_config(&r.config_content).unwrap();
        assert_eq!(cfg.dev_server.port, 5199);
        assert_eq!(cfg.out_dir, std::path::PathBuf::from("build"));
        assert_eq!(cfg.define.get("__APP__").map(String::as_str), Some("\"x\""));
        assert_eq!(cfg.build.target.as_deref(), Some("es2020"));
        assert_eq!(cfg.resolve_alias[0].to, "src");
    }

    #[test]
    fn vite_config_with_nested_objects_and_trailing_content_terminates() {
        // Regression: the old scanner spun forever on `}` / `[` it could not
        // consume as a key.
        let dir = project(&[(
            "vite.config.js",
            "export default { server: { proxy: { '/a': { target: 'http://x', changeOrigin: true } } }, plugins: [a(), b({x: [1,2]})], }",
        )]);
        let r = migrate_config(dir.path()).unwrap();
        assert_eq!(r.config["proxy"][0]["target"], json!("http://x"));
    }

    #[test]
    fn webpack_settings_are_mapped() {
        let dir = project(&[
            (
                "webpack.config.js",
                r#"const path = require('path');
const webpack = require('webpack');
module.exports = {
  entry: './src/app.js',
  output: { path: path.resolve(__dirname, 'out') },
  devServer: { port: 8081, host: '0.0.0.0', hot: false },
  resolve: { alias: { '@': path.resolve(__dirname, 'src') }, extensions: ['.js', '.jsx'] },
  devtool: false,
  plugins: [new webpack.DefinePlugin({ VERSION: JSON.stringify('1'), DEBUG: true })],
};"#,
            ),
            ("src/app.js", ""),
        ]);
        let r = migrate_config(dir.path()).unwrap();
        let c = &r.config;
        assert_eq!(c["entry"], json!(["src/app.js"]));
        assert_eq!(c["outDir"], json!("out"));
        assert_eq!(c["devServer"]["port"], json!(8081));
        assert_eq!(c["devServer"]["hmr"], json!(false));
        assert_eq!(c["resolveAlias"][0]["to"], json!("src"));
        assert_eq!(c["sourceMaps"], json!(false));
        assert_eq!(c["define"]["DEBUG"], json!("true"));
        assert!(
            r.warnings.iter().any(|w| w.contains("VERSION")),
            "non-literal DefinePlugin value must be reported: {:?}",
            r.warnings
        );
    }

    #[test]
    fn cra_and_next_samples() {
        let dir = project(&[
            (
                "package.json",
                r#"{"dependencies":{"react":"18","react-scripts":"5"},"proxy":"http://localhost:5000","homepage":"https://x.io/app"}"#,
            ),
            (".env", "PORT=4321\n"),
            ("src/index.js", ""),
        ]);
        let r = migrate_config(dir.path()).unwrap();
        assert_eq!(r.config["framework"], json!("react"));
        assert_eq!(r.config["devServer"]["port"], json!(4321));
        assert_eq!(r.config["base"], json!("/app"));
        assert_eq!(
            r.config["proxy"][0]["target"],
            json!("http://localhost:5000")
        );

        let dir = project(&[
            (
                "next.config.js",
                "module.exports = { basePath: '/docs', distDir: 'dist', env: { A: 'b' }, images: {} }",
            ),
            (
                "package.json",
                r#"{"dependencies":{"next":"14","react":"18"}}"#,
            ),
        ]);
        let r = migrate_config(dir.path()).unwrap();
        assert_eq!(r.config["framework"], json!("next"));
        assert_eq!(r.config["base"], json!("/docs"));
        assert_eq!(r.config["outDir"], json!("dist"));
        assert_eq!(r.config["define"]["process.env.A"], json!("\"b\""));
        assert!(r.warnings.iter().any(|w| w.contains("images")));
    }

    #[test]
    fn nothing_to_migrate_is_an_error() {
        let dir = project(&[("package.json", "{}")]);
        assert!(migrate_config(dir.path()).is_err());
    }
}
