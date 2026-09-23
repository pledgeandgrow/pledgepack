// Framework detection — analyzes existing project files to determine framework,
// CSS preprocessor, language, and routing setup.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedFramework {
    React,
    Vue,
    Svelte,
    Solid,
    Next,
    Remix,
    Astro,
    Qwik,
    Nuxt,
    Angular,
    Tanstack,
    Pledge,
    Vanilla,
}

impl DetectedFramework {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::React => "react",
            Self::Vue => "vue",
            Self::Svelte => "svelte",
            Self::Solid => "solid",
            Self::Next => "next",
            Self::Remix => "remix",
            Self::Astro => "astro",
            Self::Qwik => "qwik",
            Self::Nuxt => "nuxt",
            Self::Angular => "angular",
            Self::Tanstack => "tanstack",
            Self::Pledge => "pledge",
            Self::Vanilla => "vanilla",
        }
    }

    pub fn pledge_framework(&self) -> &'static str {
        match self {
            Self::React => "react",
            Self::Next => "next",
            Self::Tanstack => "tanstack",
            Self::Pledge => "pledge",
            Self::Vue | Self::Nuxt => "vue",
            Self::Svelte => "svelte",
            Self::Solid => "solid",
            Self::Remix => "react",
            Self::Astro => "astro",
            Self::Qwik => "qwik",
            Self::Angular => "angular",
            Self::Vanilla => "vanilla",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProjectDetection {
    pub framework: DetectedFramework,
    pub typescript: bool,
    pub css_preprocessor: CssPreprocessor,
    pub has_routing: bool,
    pub has_state_management: bool,
    pub package_manager: PackageManager,
    pub build_tool: BuildTool,
    pub entry_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CssPreprocessor {
    None,
    Sass,
    Less,
    Stylus,
    Tailwind,
    UnoCss,
    PandaCss,
    VanillaExtract,
}

impl CssPreprocessor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sass => "sass",
            Self::Less => "less",
            Self::Stylus => "stylus",
            Self::Tailwind => "tailwind",
            Self::UnoCss => "unocss",
            Self::PandaCss => "panda-css",
            Self::VanillaExtract => "vanilla-extract",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    Yarn,
    Pnpm,
    Bun,
}

impl PackageManager {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Yarn => "yarn",
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
        }
    }

    pub fn install_cmd(&self) -> &'static str {
        match self {
            Self::Npm => "npm install",
            Self::Yarn => "yarn",
            Self::Pnpm => "pnpm install",
            Self::Bun => "bun install",
        }
    }

    /// Install-as-devDependency command (e.g. `pnpm add -D pledgepack`).
    /// `install_cmd` lacks the flag, so this variant exists for tools like
    /// pledgepack that belong in devDependencies.
    pub fn install_dev_cmd(&self) -> &'static str {
        match self {
            Self::Npm => "npm install -D",
            Self::Yarn => "yarn add -D",
            Self::Pnpm => "pnpm add -D",
            Self::Bun => "bun add -d",
        }
    }

    pub fn dev_cmd(&self) -> &'static str {
        match self {
            Self::Npm => "npx",
            Self::Yarn => "yarn",
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildTool {
    Vite,
    Webpack,
    Cra,
    Next,
    Remix,
    Astro,
    Nuxt,
    Angular,
    Unknown,
}

impl BuildTool {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Vite => "vite",
            Self::Webpack => "webpack",
            Self::Cra => "create-react-app",
            Self::Next => "next",
            Self::Remix => "remix",
            Self::Astro => "astro",
            Self::Nuxt => "nuxt",
            Self::Angular => "angular",
            Self::Unknown => "unknown",
        }
    }
}

/// Detect project framework and configuration from existing files.
pub fn detect_project(root: &Path) -> ProjectDetection {
    let pkg_json = read_package_json(root);
    let deps = pkg_json
        .as_ref()
        .map(|p| {
            let mut all = std::collections::HashMap::new();
            if let Some(obj) = p.get("dependencies").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    if let Some(s) = v.as_str() {
                        all.insert(k.clone(), s.to_string());
                    }
                }
            }
            if let Some(obj) = p.get("devDependencies").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    if let Some(s) = v.as_str() {
                        all.insert(k.clone(), s.to_string());
                    }
                }
            }
            all
        })
        .unwrap_or_default();

    // Detect framework
    let framework = if deps.contains_key("next") {
        DetectedFramework::Next
    } else if deps.contains_key("@remix-run/react") {
        DetectedFramework::Remix
    } else if deps.contains_key("astro") {
        DetectedFramework::Astro
    } else if deps.contains_key("@builder.io/qwik") {
        DetectedFramework::Qwik
    } else if deps.contains_key("nuxt") || deps.contains_key("nuxt3") {
        DetectedFramework::Nuxt
    } else if deps.contains_key("@angular/core") {
        DetectedFramework::Angular
    } else if deps.contains_key("@tanstack/react-router") {
        DetectedFramework::Tanstack
    } else if deps.contains_key("pledgestack") || deps.contains_key("@pledgestack/core") {
        DetectedFramework::Pledge
    } else if deps.contains_key("solid-js") {
        DetectedFramework::Solid
    } else if deps.contains_key("svelte") {
        DetectedFramework::Svelte
    } else if deps.contains_key("vue") {
        DetectedFramework::Vue
    } else if deps.contains_key("react") {
        DetectedFramework::React
    } else {
        DetectedFramework::Vanilla
    };

    // Detect entry file
    let entry_file = detect_entry_file(root, &framework);

    // Detect TypeScript: a typescript dependency, a tsconfig, a TS entry
    // (.ts/.tsx), or a TS build-tool config (vite.config.ts, ...).
    let typescript = deps.contains_key("typescript")
        || root.join("tsconfig.json").exists()
        || entry_file.ends_with(".ts")
        || entry_file.ends_with(".tsx")
        || [
            "vite.config.ts",
            "webpack.config.ts",
            "next.config.ts",
            "pledge.config.ts",
        ]
        .iter()
        .any(|f| root.join(f).exists());

    // Detect CSS preprocessor
    let css_preprocessor = if deps.contains_key("tailwindcss") {
        CssPreprocessor::Tailwind
    } else if deps.contains_key("unocss") || deps.contains_key("@unocss/core") {
        CssPreprocessor::UnoCss
    } else if deps.contains_key("@pandacss/dev") {
        CssPreprocessor::PandaCss
    } else if deps.contains_key("@vanilla-extract/css") {
        CssPreprocessor::VanillaExtract
    } else if deps.contains_key("sass") || deps.contains_key("node-sass") {
        CssPreprocessor::Sass
    } else if deps.contains_key("less") {
        CssPreprocessor::Less
    } else if deps.contains_key("stylus") {
        CssPreprocessor::Stylus
    } else {
        CssPreprocessor::None
    };

    // Detect routing
    let has_routing = deps.contains_key("react-router-dom")
        || deps.contains_key("@tanstack/react-router")
        || deps.contains_key("vue-router")
        || deps.contains_key("@remix-run/react")
        || deps.contains_key("next")
        || deps.contains_key("@angular/router");

    // Detect state management
    let has_state_management = deps.contains_key("redux")
        || deps.contains_key("@reduxjs/toolkit")
        || deps.contains_key("zustand")
        || deps.contains_key("jotai")
        || deps.contains_key("@tanstack/react-query")
        || deps.contains_key("mobx")
        || deps.contains_key("pinia")
        || deps.contains_key("nanostores");

    // Detect package manager
    let package_manager = detect_package_manager(root);

    // Detect build tool
    let build_tool = if deps.contains_key("vite")
        || root.join("vite.config.ts").exists()
        || root.join("vite.config.js").exists()
    {
        BuildTool::Vite
    } else if deps.contains_key("next") {
        BuildTool::Next
    } else if deps.contains_key("@remix-run/dev") {
        BuildTool::Remix
    } else if deps.contains_key("astro") {
        BuildTool::Astro
    } else if deps.contains_key("nuxt") || deps.contains_key("nuxt3") {
        BuildTool::Nuxt
    } else if deps.contains_key("@angular/cli")
        || deps.contains_key("@angular-devkit/build-angular")
    {
        BuildTool::Angular
    } else if deps.contains_key("webpack") || deps.contains_key("webpack-cli") {
        BuildTool::Webpack
    } else if root.join("config-overrides").exists()
        || root.join("react-scripts").exists()
        || deps.contains_key("react-scripts")
    {
        BuildTool::Cra
    } else {
        BuildTool::Unknown
    };

    ProjectDetection {
        framework,
        typescript,
        css_preprocessor,
        has_routing,
        has_state_management,
        package_manager,
        build_tool,
        entry_file,
    }
}

fn detect_package_manager(root: &Path) -> PackageManager {
    if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
        PackageManager::Bun
    } else if root.join("pnpm-lock.yaml").exists() {
        PackageManager::Pnpm
    } else if root.join("yarn.lock").exists() {
        PackageManager::Yarn
    } else {
        PackageManager::Npm
    }
}

/// First module `<script src="...">` referenced by the project's `index.html`
/// (Vite-style HTML entry), when that file exists on disk.
pub fn entry_from_html(root: &Path) -> Option<String> {
    let html = std::fs::read_to_string(root.join("index.html")).ok()?;
    let mut rest = html.as_str();
    while let Some(pos) = rest.find("<script") {
        let tag_end = rest[pos..].find('>').map(|e| pos + e)?;
        let tag = &rest[pos..tag_end];
        if let Some(src_pos) = tag.find("src=") {
            let after = &tag[src_pos + 4..];
            let quote = after.chars().next()?;
            if quote == '"' || quote == '\'' {
                let val = &after[1..];
                if let Some(end) = val.find(quote) {
                    let src = val[..end].trim_start_matches('/').trim_start_matches("./");
                    if !src.starts_with("http") && root.join(src).is_file() {
                        return Some(src.to_string());
                    }
                }
            }
        }
        rest = &rest[tag_end..];
    }
    None
}

fn detect_entry_file(root: &Path, framework: &DetectedFramework) -> String {
    if let Some(e) = entry_from_html(root) {
        return e;
    }
    let candidates = match framework {
        DetectedFramework::Next | DetectedFramework::Remix | DetectedFramework::Pledge => {
            vec![
                "src/app/root.tsx",
                "src/app.tsx",
                "app/layout.tsx",
                "app/page.tsx",
                "src/main.tsx",
                "pages/index.tsx",
                "src/index.tsx",
            ]
        }
        DetectedFramework::Angular => vec!["src/main.ts", "src/main.tsx"],
        DetectedFramework::Astro => vec!["src/pages/index.astro", "src/index.astro"],
        DetectedFramework::Nuxt => vec!["src/app.vue", "src/main.ts", "src/index.ts"],
        _ => vec![
            "src/index.tsx",
            "src/index.ts",
            "src/main.tsx",
            "src/main.ts",
            "src/index.jsx",
            "src/index.js",
            "src/main.jsx",
            "src/main.js",
        ],
    };

    for candidate in candidates {
        if root.join(candidate).exists() {
            return candidate.to_string();
        }
    }

    "src/index.tsx".to_string()
}

fn read_package_json(root: &Path) -> Option<serde_json::Value> {
    let pkg_path = root.join("package.json");
    if !pkg_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&pkg_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Generate a pledge.config.ts based on detected project settings.
pub fn generate_config(detection: &ProjectDetection) -> String {
    let framework = detection.framework.pledge_framework();
    let entry = &detection.entry_file;

    let mut plugins = Vec::new();
    let mut extra_fields = String::new();

    // Add CSS framework config
    match detection.css_preprocessor {
        CssPreprocessor::Tailwind => {
            extra_fields.push_str("  // Tailwind CSS: PostCSS plugin handles processing\n");
        }
        CssPreprocessor::UnoCss => {
            plugins.push("\"@unocss/plugin-pledge\"".to_string());
        }
        CssPreprocessor::PandaCss => {
            extra_fields.push_str("  // Panda CSS: uses build-time codegen\n");
        }
        CssPreprocessor::VanillaExtract => {
            extra_fields.push_str("  // Vanilla Extract: .css.ts files processed at build time\n");
        }
        _ => {}
    }

    // Add proxy config if it looks like a full-stack app
    extra_fields.push_str("  devServer: {\n    port: 3000,\n    hmr: true,\n  },\n");

    let plugins_str = if plugins.is_empty() {
        String::new()
    } else {
        format!("\n  plugins: [{}],", plugins.join(", "))
    };

    format!(
        r#"import {{ defineConfig }} from 'pledgepack';

export default defineConfig({{
  entry: ['{}'],
  framework: '{}',{plugins}
{extra_fields}}});
"#,
        entry,
        framework,
        plugins = plugins_str,
        extra_fields = extra_fields.trim_end_matches(",\n"),
    )
}

/// Result of [`add_missing_scripts`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptsPatch {
    /// The (possibly unchanged) package.json text.
    pub text: String,
    /// Scripts that were added.
    pub added: Vec<String>,
    /// `(name, existing command)` for scripts that already existed and were
    /// left untouched.
    pub kept: Vec<(String, String)>,
}

/// Add each `(name, command)` to package.json's `scripts` unless a script with
/// that name already exists. The edit is textual so key order, indentation and
/// formatting of the rest of the file are preserved. Returns `None` if
/// `package_json` is not a JSON object (or `scripts` is not an object).
pub fn add_missing_scripts(package_json: &str, wanted: &[(&str, &str)]) -> Option<ScriptsPatch> {
    add_missing_entries(package_json, "scripts", wanted)
}

/// Same as [`add_missing_scripts`] but for an arbitrary top-level object
/// member of package.json (e.g. `"devDependencies"`).
pub fn add_missing_entries(
    package_json: &str,
    section: &str,
    wanted: &[(&str, &str)],
) -> Option<ScriptsPatch> {
    let value: serde_json::Value = serde_json::from_str(package_json).ok()?;
    let obj = value.as_object()?;
    let existing = match obj.get(section) {
        None => None,
        Some(serde_json::Value::Object(m)) => Some(m),
        Some(_) => return None,
    };

    let mut patch = ScriptsPatch {
        text: package_json.to_string(),
        ..Default::default()
    };
    let mut to_add: Vec<(&str, &str)> = Vec::new();
    for (name, cmd) in wanted {
        match existing.and_then(|m| m.get(*name)) {
            Some(v) => patch
                .kept
                .push((name.to_string(), v.as_str().unwrap_or("").to_string())),
            None => to_add.push((name, cmd)),
        }
    }
    if to_add.is_empty() {
        return Some(patch);
    }

    let nl = if package_json.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let unit = package_json
        .lines()
        .find_map(|l| {
            let ws: String = l.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            (!ws.is_empty() && l.trim_start().starts_with('"')).then_some(ws)
        })
        .unwrap_or_else(|| "  ".to_string());
    let entries = |indent: &str| -> String {
        to_add
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}{}: {}",
                    indent,
                    serde_json::to_string(k).unwrap_or_default(),
                    serde_json::to_string(v).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join(&format!(",{}", nl))
    };
    let inner_indent = format!("{unit}{unit}");

    let text = package_json;
    let new_text = if existing.is_some() {
        let key_pos = find_top_level_key(text, section)?;
        let open = key_pos + text[key_pos..].find('{')?;
        let close = matching_brace(text, open)?;
        let body = text[open + 1..close].trim_end();
        if body.trim().is_empty() {
            format!(
                "{}{{{nl}{}{nl}{}}}{}",
                &text[..open],
                entries(&inner_indent),
                unit,
                &text[close + 1..],
                nl = nl
            )
        } else {
            let keep_end = open + 1 + body.len();
            format!(
                "{},{nl}{}{}",
                &text[..keep_end],
                entries(&inner_indent),
                &text[keep_end..],
                nl = nl
            )
        }
    } else {
        let close = text.rfind('}')?;
        let before = text[..close].trim_end();
        let sep = if before.ends_with('{') { "" } else { "," };
        format!(
            "{}{sep}{nl}{unit}\"{section}\": {{{nl}{}{nl}{unit}}}{nl}{}",
            before,
            entries(&inner_indent),
            &text[close..],
            sep = sep,
            nl = nl,
            unit = unit,
            section = section
        )
    };
    patch.text = new_text;
    patch.added = to_add.iter().map(|(k, _)| k.to_string()).collect();
    Some(patch)
}

/// Byte offset of the `"key"` token for a depth-1 property of the root object.
fn find_top_level_key(text: &str, key: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let needle = format!("\"{}\"", key);
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                if depth == 1 && text[i..].starts_with(&needle) {
                    let after = text[i + needle.len()..].trim_start();
                    if after.starts_with(':') {
                        return Some(i);
                    }
                }
                i = string_end(bytes, i);
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index of the closing quote of the string starting at `start`.
fn string_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'"' => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len().saturating_sub(1)
}

/// Index of the `}` matching the `{` at `open`.
fn matching_brace(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = string_end(bytes, i),
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod scripts_tests {
    use super::*;

    const WANTED: &[(&str, &str)] = &[
        ("dev", "pledgepack dev"),
        ("build", "pledgepack build"),
        ("preview", "pledgepack preview"),
    ];

    #[test]
    fn adds_a_scripts_block_when_missing_and_stays_valid_json() {
        let src = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"react\": \"18\"\n  }\n}\n";
        let p = add_missing_scripts(src, WANTED).unwrap();
        let v: serde_json::Value = serde_json::from_str(&p.text).unwrap();
        assert_eq!(v["scripts"]["dev"], "pledgepack dev");
        assert_eq!(v["scripts"]["preview"], "pledgepack preview");
        assert_eq!(v["dependencies"]["react"], "18");
        assert_eq!(p.added, vec!["dev", "build", "preview"]);
        // untouched key order
        assert!(p.text.find("\"name\"").unwrap() < p.text.find("\"dependencies\"").unwrap());
    }

    #[test]
    fn never_overwrites_existing_scripts() {
        let src = "{\n  \"scripts\": {\n    \"dev\": \"vite\",\n    \"test\": \"vitest\"\n  }\n}";
        let p = add_missing_scripts(src, WANTED).unwrap();
        let v: serde_json::Value = serde_json::from_str(&p.text).unwrap();
        assert_eq!(v["scripts"]["dev"], "vite");
        assert_eq!(v["scripts"]["test"], "vitest");
        assert_eq!(v["scripts"]["build"], "pledgepack build");
        assert_eq!(p.added, vec!["build", "preview"]);
        assert_eq!(p.kept, vec![("dev".to_string(), "vite".to_string())]);
    }

    #[test]
    fn handles_empty_scripts_and_no_changes() {
        let p = add_missing_scripts("{\"scripts\": {}}", WANTED).unwrap();
        let v: serde_json::Value = serde_json::from_str(&p.text).unwrap();
        assert_eq!(v["scripts"]["build"], "pledgepack build");
        let all = "{\"scripts\":{\"dev\":\"a\",\"build\":\"b\",\"preview\":\"c\"}}";
        let p = add_missing_scripts(all, WANTED).unwrap();
        assert!(p.added.is_empty());
        assert_eq!(p.text, all);
        assert!(add_missing_scripts("[1]", WANTED).is_none());
    }

    #[test]
    fn scripts_key_inside_dependencies_is_not_mistaken_for_the_block() {
        let src = "{\"dependencies\":{\"scripts\":\"1.0.0\"},\"scripts\":{\"x\":\"y\"}}";
        let p = add_missing_scripts(src, WANTED).unwrap();
        let v: serde_json::Value = serde_json::from_str(&p.text).unwrap();
        assert_eq!(v["scripts"]["dev"], "pledgepack dev");
        assert_eq!(v["dependencies"]["scripts"], "1.0.0");
    }
}
