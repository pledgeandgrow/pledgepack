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

    // Detect TypeScript
    let typescript = deps.contains_key("typescript")
        || root.join("tsconfig.json").exists()
        || root.join("jsconfig.json").exists();

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

    // Detect entry file
    let entry_file = detect_entry_file(root, &framework);

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

fn detect_entry_file(root: &Path, framework: &DetectedFramework) -> String {
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
