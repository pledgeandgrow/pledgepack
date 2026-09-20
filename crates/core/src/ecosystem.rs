// Ecosystem & Extensibility features: #94, #97, #98, #99, #100

use crate::config::{
    Framework, PledgeConfig, PluginPreset, TransformPipelineConfig, WorkspaceConfig,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

// ── Feature 94: Plugin Preset System ──────────────────────────────────

/// Built-in plugin presets. Returns a Vec because PluginPreset contains
/// heap-allocated Strings that can't be used in statics.
pub fn builtin_presets() -> Vec<PluginPreset> {
    vec![
        PluginPreset {
            name: "react".to_string(),
            plugins: vec![],
            config_defaults: serde_json::json!({"framework": "react", "jsx": "classic"}),
            description: "React preset — React 18+ with classic JSX runtime".to_string(),
        },
        PluginPreset {
            name: "tailwind".to_string(),
            plugins: vec![],
            config_defaults: serde_json::json!({"css": {"tailwind": true}}),
            description: "Tailwind CSS preset — Tailwind v4 with @apply".to_string(),
        },
        PluginPreset {
            name: "solid".to_string(),
            plugins: vec![],
            config_defaults: serde_json::json!({"framework": "solid", "jsx": "automatic"}),
            description: "SolidJS preset — Solid.js with automatic JSX runtime".to_string(),
        },
        PluginPreset {
            name: "vue".to_string(),
            plugins: vec![],
            config_defaults: serde_json::json!({"framework": "vue"}),
            description: "Vue preset — Vue 3 SFC compilation".to_string(),
        },
        PluginPreset {
            name: "svelte".to_string(),
            plugins: vec![],
            config_defaults: serde_json::json!({"framework": "svelte"}),
            description: "Svelte preset — Svelte 5 SFC compilation".to_string(),
        },
        PluginPreset {
            name: "astro".to_string(),
            plugins: vec![],
            config_defaults: serde_json::json!({"framework": "auto"}),
            description: "Astro preset — Astro island architecture".to_string(),
        },
    ]
}

/// Parse a framework string into Framework enum
fn parse_framework(s: &str) -> Option<Framework> {
    match s {
        "react" => Some(Framework::React),
        "vue" => Some(Framework::Vue),
        "svelte" => Some(Framework::Svelte),
        "solid" => Some(Framework::Solid),
        "next" => Some(Framework::Next),
        "tanstack" => Some(Framework::TanStack),
        "astro" => Some(Framework::Astro),
        "pledge" => Some(Framework::Pledge),
        "auto" => Some(Framework::Auto),
        _ => None,
    }
}

pub fn resolve_preset(name: &str) -> Option<PluginPreset> {
    for preset in builtin_presets() {
        if preset.name == name {
            return Some(preset);
        }
    }
    let pkg = format!("pledgepack-preset-{}", name);
    let path = PathBuf::from("node_modules").join(&pkg).join("preset.json");
    if path.is_file()
        && let Ok(content) = std::fs::read_to_string(&path)
        && let Ok(preset) = serde_json::from_str::<PluginPreset>(&content)
    {
        info!("Loaded community preset '{}' from {}", name, crate::display_path(&path));
        return Some(preset);
    }
    warn!("Preset '{}' not found", name);
    None
}

pub fn apply_presets(config: &mut PledgeConfig) -> Result<()> {
    if config.presets.is_empty() {
        return Ok(());
    }
    let mut all_plugins = config.plugins.clone();
    for name in &config.presets {
        if let Some(preset) = resolve_preset(name) {
            info!("Applying preset: {} — {}", preset.name, preset.description);
            all_plugins.extend(preset.plugins);
            if let Some(obj) = preset.config_defaults.as_object()
                && let Some(fw) = obj.get("framework").and_then(|v| v.as_str())
                && let Some(framework) = parse_framework(fw)
            {
                config.framework = framework;
            }
        }
    }
    config.plugins = all_plugins;
    Ok(())
}

pub fn list_available_presets() -> Vec<String> {
    let mut names: Vec<String> = builtin_presets().iter().map(|p| p.name.clone()).collect();
    let nm = PathBuf::from("node_modules");
    if nm.is_dir()
        && let Ok(entries) = std::fs::read_dir(&nm)
    {
        for entry in entries.flatten() {
            if let Some(n) = entry.file_name().to_str()
                && n.starts_with("pledgepack-preset-")
            {
                let s = n.trim_start_matches("pledgepack-preset-").to_string();
                if !names.contains(&s) {
                    names.push(s);
                }
            }
        }
    }
    names
}

// ── Feature 97: Custom Transformer Pipeline ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformStep {
    pub name: String,
    pub built_in: bool,
    #[serde(default)]
    pub config: serde_json::Value,
}

pub fn build_pipeline(config: &TransformPipelineConfig) -> Vec<TransformStep> {
    let mut steps = Vec::new();
    if config.replace_default {
        for name in &config.pipeline {
            steps.push(TransformStep {
                name: name.clone(),
                built_in: matches!(name.as_str(), "oxc" | "minify" | "tree-shake"),
                config: serde_json::Value::Null,
            });
        }
    } else {
        steps.push(TransformStep {
            name: "oxc".into(),
            built_in: true,
            config: serde_json::Value::Null,
        });
        for name in &config.pipeline {
            if !matches!(name.as_str(), "oxc" | "minify" | "tree-shake") {
                steps.push(TransformStep {
                    name: name.clone(),
                    built_in: false,
                    config: serde_json::Value::Null,
                });
            }
        }
        steps.push(TransformStep {
            name: "minify".into(),
            built_in: true,
            config: serde_json::Value::Null,
        });
        steps.push(TransformStep {
            name: "tree-shake".into(),
            built_in: true,
            config: serde_json::Value::Null,
        });
    }
    steps
}

pub fn default_pipeline() -> Vec<TransformStep> {
    vec![
        TransformStep {
            name: "oxc".into(),
            built_in: true,
            config: serde_json::Value::Null,
        },
        TransformStep {
            name: "minify".into(),
            built_in: true,
            config: serde_json::Value::Null,
        },
        TransformStep {
            name: "tree-shake".into(),
            built_in: true,
            config: serde_json::Value::Null,
        },
    ]
}

// ── Feature 98: Workspace-Aware Resolution ────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacePackage {
    pub name: String,
    pub path: PathBuf,
    pub version: String,
    pub main: Option<String>,
    pub module: Option<String>,
    pub types: Option<String>,
    pub exports: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub root: PathBuf,
    pub packages: std::collections::HashMap<String, WorkspacePackage>,
    pub package_manager: String,
}

pub fn detect_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(start)
    };
    loop {
        let pkg = current.join("package.json");
        if pkg.is_file()
            && let Ok(content) = std::fs::read_to_string(&pkg)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            && json.get("workspaces").is_some()
        {
            return Some(current);
        }
        if current.join("pnpm-workspace.yaml").is_file() || current.join("lerna.json").is_file() {
            return Some(current);
        }
        if !current.pop() {
            break;
        }
    }
    None
}

fn detect_package_manager(root: &Path) -> String {
    if root.join("pnpm-lock.yaml").is_file() {
        "pnpm".into()
    } else if root.join("yarn.lock").is_file() {
        "yarn".into()
    } else {
        "npm".into()
    }
}

fn parse_workspace_patterns(root: &Path) -> Vec<String> {
    let pkg = root.join("package.json");
    if !pkg.is_file() {
        return vec![];
    }
    let content = match std::fs::read_to_string(&pkg) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(_) => return vec![],
    };
    match json.get("workspaces") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(serde_json::Value::Object(obj)) => obj
            .get("packages")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        _ => vec![],
    }
}

fn parse_pnpm_patterns(root: &Path) -> Vec<String> {
    let path = root.join("pnpm-workspace.yaml");
    if !path.is_file() {
        return vec![];
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut patterns = Vec::new();
    let mut in_pkgs = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("packages:") {
            in_pkgs = true;
            continue;
        }
        if in_pkgs {
            if t.starts_with('-') {
                patterns.push(
                    t.trim_start_matches('-')
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_string(),
                );
            } else if !t.is_empty() && !t.starts_with('#') {
                in_pkgs = false;
            }
        }
    }
    patterns
}

fn expand_glob(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            let dir = root.join(parts[0].trim_end_matches('/'));
            if dir.is_dir()
                && let Ok(entries) = std::fs::read_dir(&dir)
            {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() && p.join("package.json").is_file() {
                        results.push(p);
                    }
                }
            }
        }
    } else {
        let p = root.join(pattern);
        if p.join("package.json").is_file() {
            results.push(p);
        }
    }
    results
}

fn parse_pkg(path: &Path) -> Option<WorkspacePackage> {
    let pj = path.join("package.json");
    if !pj.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&pj).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    Some(WorkspacePackage {
        name: json
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        path: path.to_path_buf(),
        version: json
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .into(),
        main: json.get("main").and_then(|v| v.as_str()).map(String::from),
        module: json
            .get("module")
            .and_then(|v| v.as_str())
            .map(String::from),
        types: json.get("types").and_then(|v| v.as_str()).map(String::from),
        exports: json.get("exports").cloned(),
    })
}

pub fn detect_workspace(start: &Path) -> Option<WorkspaceInfo> {
    let root = detect_workspace_root(start)?;
    let pm = detect_package_manager(&root);
    let patterns = if pm == "pnpm" {
        let mut p = parse_pnpm_patterns(&root);
        if p.is_empty() {
            p = parse_workspace_patterns(&root);
        }
        p
    } else {
        parse_workspace_patterns(&root)
    };

    let mut packages = std::collections::HashMap::new();
    for pattern in &patterns {
        for pkg_path in expand_glob(&root, pattern) {
            if let Some(pkg) = parse_pkg(&pkg_path)
                && !pkg.name.is_empty()
            {
                packages.insert(pkg.name.clone(), pkg);
            }
        }
    }
    info!(
        "Detected workspace at {} with {} packages ({})",
        crate::display_path(&root),
        packages.len(),
        pm
    );
    Some(WorkspaceInfo {
        root,
        packages,
        package_manager: pm,
    })
}

pub fn resolve_workspace_import(specifier: &str, ws: &WorkspaceInfo) -> Option<PathBuf> {
    let (pkg_name, subpath) = if let Some(rest) = specifier.strip_prefix('@') {
        if let Some(pos) = rest.find('/') {
            (&specifier[..pos + 1], Some(&rest[pos + 1..]))
        } else {
            (specifier, None)
        }
    } else if let Some(pos) = specifier.find('/') {
        (&specifier[..pos], Some(&specifier[pos + 1..]))
    } else {
        (specifier, None)
    };

    let pkg = ws.packages.get(pkg_name)?;
    if let Some(sub) = subpath {
        if let Some(exports) = &pkg.exports
            && let Some(obj) = exports.as_object()
        {
            let key = format!("./{}", sub);
            if let Some(v) = obj.get(&key).and_then(|v| v.as_str()) {
                let p = pkg.path.join(v);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
        let direct = pkg.path.join(sub);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in [".ts", ".tsx", ".js", ".jsx", ".mjs", ".json"] {
            let p = PathBuf::from(format!("{}{}", direct.display(), ext));
            if p.is_file() {
                return Some(p);
            }
        }
    } else {
        if let Some(m) = &pkg.module {
            let p = pkg.path.join(m);
            if p.is_file() {
                return Some(p);
            }
        }
        if let Some(m) = &pkg.main {
            let p = pkg.path.join(m);
            if p.is_file() {
                return Some(p);
            }
        }
        for idx in ["index.ts", "index.tsx", "index.js", "index.jsx"] {
            let p = pkg.path.join(idx);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

// ── Feature 99: Cross-Package HMR ─────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct HmrDependencyMap {
    file_to_packages: std::collections::HashMap<PathBuf, Vec<String>>,
    package_to_files: std::collections::HashMap<String, Vec<PathBuf>>,
}

impl HmrDependencyMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_file(&mut self, file: PathBuf, pkg: &str) {
        self.file_to_packages
            .entry(file.clone())
            .or_default()
            .push(pkg.into());
        self.package_to_files
            .entry(pkg.into())
            .or_default()
            .push(file);
    }

    pub fn compute_hmr_set(&self, changed: &Path, _ws: &WorkspaceInfo) -> Vec<PathBuf> {
        let mut set = vec![changed.to_path_buf()];
        for (pkg_name, files) in &self.package_to_files {
            if files.iter().any(|f| f == changed) {
                for f in files {
                    if !set.contains(f) {
                        set.push(f.clone());
                    }
                }
                for (other_name, other_files) in &self.package_to_files {
                    if other_name != pkg_name {
                        for of in other_files {
                            if let Ok(content) = std::fs::read_to_string(of)
                                && (content.contains(&format!("from \"{}\"", pkg_name))
                                    || content.contains(&format!("from '{}'", pkg_name))
                                    || content.contains(&format!("import(\"{}\")", pkg_name))
                                    || content.contains(&format!("import('{}')", pkg_name)))
                            {
                                if !set.contains(of) {
                                    set.push(of.clone());
                                }
                                break;
                            }
                        }
                    }
                }
                break;
            }
        }
        if let Some(deps) = self.file_to_packages.get(changed) {
            for dep in deps {
                if let Some(files) = self.package_to_files.get(dep) {
                    for f in files {
                        if !set.contains(f) {
                            set.push(f.clone());
                        }
                    }
                }
            }
        }
        set
    }
}

pub fn build_hmr_map(ws: &WorkspaceInfo) -> HmrDependencyMap {
    let mut map = HmrDependencyMap::new();
    for (name, pkg) in &ws.packages {
        let src = pkg.path.join("src");
        let dir = if src.is_dir() { &src } else { &pkg.path };
        scan_files(dir, name, &mut map);
    }
    map
}

fn scan_files(dir: &Path, pkg: &str, map: &mut HmrDependencyMap) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                scan_files(&p, pkg, map);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str())
                && matches!(ext, "ts" | "tsx" | "js" | "jsx" | "mjs" | "vue" | "svelte")
            {
                map.register_file(p, pkg);
            }
        }
    }
}

// ── Feature 100: Shared Build Cache ───────────────────────────────────

pub fn resolve_shared_cache_dir(ws: &WorkspaceInfo, config: &WorkspaceConfig) -> PathBuf {
    if config.shared_cache {
        ws.root.join(".pledge").join("cache")
    } else {
        PathBuf::from("node_modules").join(".pledge-cache")
    }
}

pub fn resolve_workspace_root(config: &WorkspaceConfig, start: &Path) -> Option<PathBuf> {
    if let Some(ref root) = config.root
        && root.is_dir()
    {
        return Some(root.clone());
    }
    detect_workspace_root(start)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_presets() {
        assert!(resolve_preset("react").is_some());
        assert!(resolve_preset("tailwind").is_some());
        assert!(resolve_preset("solid").is_some());
        assert!(resolve_preset("vue").is_some());
        assert!(resolve_preset("svelte").is_some());
        assert!(resolve_preset("astro").is_some());
        assert!(resolve_preset("nonexistent").is_none());
    }

    #[test]
    fn test_list_presets() {
        let names = list_available_presets();
        assert!(names.contains(&"react".to_string()));
        assert!(names.contains(&"tailwind".to_string()));
    }

    #[test]
    fn test_apply_presets() {
        let mut config = PledgeConfig {
            presets: vec!["react".into()],
            ..Default::default()
        };
        apply_presets(&mut config).unwrap();
        assert_eq!(config.framework, crate::config::Framework::React);
    }

    #[test]
    fn test_pipeline_default() {
        let cfg = TransformPipelineConfig::default();
        let steps = build_pipeline(&cfg);
        assert!(steps.iter().any(|s| s.name == "oxc"));
        assert!(steps.iter().any(|s| s.name == "minify"));
    }

    #[test]
    fn test_pipeline_replace() {
        let cfg = TransformPipelineConfig {
            pipeline: vec!["oxc".into()],
            replace_default: true,
        };
        let steps = build_pipeline(&cfg);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].name, "oxc");
    }

    #[test]
    fn test_pipeline_custom_insert() {
        let cfg = TransformPipelineConfig {
            pipeline: vec!["my-transform".into()],
            replace_default: false,
        };
        let steps = build_pipeline(&cfg);
        let has_custom = steps
            .iter()
            .any(|s| s.name == "my-transform" && !s.built_in);
        assert!(has_custom);
        let oxc_pos = steps.iter().position(|s| s.name == "oxc").unwrap();
        let custom_pos = steps.iter().position(|s| s.name == "my-transform").unwrap();
        assert!(custom_pos > oxc_pos);
    }

    #[test]
    fn test_default_pipeline() {
        let steps = default_pipeline();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "oxc");
    }

    #[test]
    fn test_hmr_map() {
        let mut map = HmrDependencyMap::new();
        map.register_file(PathBuf::from("/pkg-a/src/index.ts"), "pkg-a");
        assert_eq!(map.get_package_files("pkg-a").len(), 1);
    }

    // Helper for tests
    impl HmrDependencyMap {
        pub fn get_package_files(&self, name: &str) -> Vec<PathBuf> {
            self.package_to_files.get(name).cloned().unwrap_or_default()
        }
    }
}
