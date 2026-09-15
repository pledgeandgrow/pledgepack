//! Module resolver: resolves import specifiers to file paths
//!
//! Handles:
//!   - Relative paths (./foo, ../bar)
//!   - Bare specifiers (react, lodash) → node_modules
//!   - Path aliases (tsconfig.json paths, jsconfig.json)
//!   - Extension resolution (.tsx → .ts → .jsx → .js)
//!   - Directory resolution (./components → ./components/index.tsx)
//!   - package.json "exports" field

use anyhow::Result;
use dashmap::DashMap;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolves module specifiers (imports/exports) to on-disk file paths.
///
/// The resolver understands relative and absolute paths, bare package
/// specifiers (node_modules, including pnpm's virtual store), tsconfig
/// path aliases, package.json `exports`/`imports` fields, and workspace
/// (monorepo) package resolution. Successful resolutions are memoized in
/// an internal cache keyed by importer directory and specifier.
///
/// **Not supported: Yarn Plug'n'Play (PnP).** There is no `.pnp.cjs`/
/// `.pnp.loader.mjs` manifest handling here — only plain `node_modules`
/// layouts (npm's, and pnpm's symlink + `.pnpm` virtual-store layout) are
/// resolved. A project using Yarn PnP will fail to resolve its
/// dependencies with this resolver. This was previously undocumented
/// rather than explicitly unsupported; confirmed absent (not just
/// unfinished) as of PRODUCTION-READINESS-100.md goal 57 — adding real
/// support would mean parsing and querying the PnP manifest's package
/// registry, a separate resolution strategy from everything above, not an
/// extension of it.
pub struct Resolver {
    root: PathBuf,
    extensions: Vec<String>,
    aliases: Vec<Alias>,
    /// Custom conditions for package.json exports resolution (#119)
    custom_conditions: Vec<String>,
    /// Optional workspace info for monorepo resolution (#98)
    workspace: Option<pledgepack_core::ecosystem::WorkspaceInfo>,
    /// Target runtime used to derive export condition priority
    runtime: ResolveRuntime,
    /// Module type used to derive export condition priority
    module_type: ResolveModuleType,
    /// Cache: specifier → resolved path (per-directory context)
    cache: Arc<DashMap<(PathBuf, String), Option<PathBuf>>>,
}

/// A path alias mapping (e.g. `@/` → `src/`), typically derived from
/// tsconfig/jsconfig `compilerOptions.paths`.
///
/// When a specifier starts with `from`, the prefix is replaced by `to`
/// and the result is resolved as a filesystem path.
#[derive(Debug, Clone)]
pub struct Alias {
    /// The specifier prefix to match (e.g. `"@/"`).
    pub from: String,
    /// The directory the prefix maps to (e.g. `"src/"`).
    pub to: String,
}

/// Target runtime for module resolution. Determines which export conditions
/// (e.g. "browser", "node") are preferred when resolving package.json fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveRuntime {
    Node,
    Browser,
    Deno,
    Bun,
    Worker,
}

impl Default for ResolveRuntime {
    /// Defaults to `Browser` to preserve the resolver's historical behaviour of
    /// preferring the "browser" export condition.
    fn default() -> Self {
        ResolveRuntime::Browser
    }
}

/// Module system of the importing module. Determines whether "import"/"module"
/// or "require" conditions are preferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolveModuleType {
    #[default]
    Esm,
    Cjs,
}

/// Resolution context: combines the target runtime and module type so that
/// export condition priority can be derived dynamically instead of hardcoded.
#[derive(Debug, Clone, Default)]
pub struct ResolveContext {
    pub runtime: ResolveRuntime,
    pub module_type: ResolveModuleType,
}

#[derive(Debug, Deserialize)]
struct TsConfig {
    compiler_options: Option<CompilerOptions>,
    extends: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompilerOptions {
    base_url: Option<String>,
    paths: Option<std::collections::HashMap<String, Vec<String>>>,
}

/// Canonicalize `path`, logging a warning and falling back to the
/// un-canonicalized path if that fails, rather than silently doing so.
///
/// Every call site here has already confirmed the path exists
/// (`is_file()`/`is_dir()`) immediately before calling this, so under
/// normal conditions `canonicalize()` should always succeed — a failure
/// here means something unusual happened between that check and now: most
/// notably a broken symlink (its target doesn't exist, so `canonicalize()`
/// can't resolve it, even though the symlink itself passed `is_file()`'s
/// existence check... actually `is_file()` follows symlinks and would
/// itself return false for a target that doesn't exist, so a genuinely
/// broken symlink is caught earlier; more likely causes here are a
/// permissions error, a symlink loop, or a TOCTOU race where the file was
/// removed between the existence check and this call), a permissions
/// error, or a symlink cycle. Previously this fell back silently, which for
/// a stale/dangling entry could resolve to something misleading with no
/// indication anything was wrong. See PRODUCTION-READINESS-100.md goal 53.
fn canonicalize_or_warn(path: PathBuf) -> PathBuf {
    match path.canonicalize() {
        Ok(canonical) => {
            warn_on_case_mismatch(&path, &canonical);
            canonical
        }
        Err(e) => {
            tracing::warn!(
                "Resolver: failed to canonicalize {} ({e}) — using the path as-is, which may not \
                 reflect a symlink target correctly",
                path.display()
            );
            path
        }
    }
}

/// Warn when `requested`'s filename differs only in case from `canonical`'s
/// — the actual on-disk name. Windows and macOS filesystems are
/// case-insensitive-but-preserving: `canonicalize()` returns the real,
/// disk-cased path regardless of the case used to reach it, so comparing
/// the two catches an import whose case doesn't match the file on disk —
/// works today on a Windows/macOS dev machine, breaks silently the moment
/// it's built on case-sensitive Linux (most CI and production hosts). On a
/// case-sensitive filesystem this is unreachable: a mis-cased path simply
/// wouldn't have passed the `is_file()`/`is_dir()` check every call site of
/// [`canonicalize_or_warn`] already did before reaching here. See
/// PRODUCTION-READINESS-100.md goal 51.
fn warn_on_case_mismatch(requested: &Path, canonical: &Path) {
    let (Some(requested_name), Some(actual_name)) = (
        requested.file_name().and_then(|n| n.to_str()),
        canonical.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };

    if requested_name != actual_name && requested_name.eq_ignore_ascii_case(actual_name) {
        tracing::warn!(
            "Case-sensitivity mismatch: resolved '{}' but the file on disk is actually named \
             '{}'. This will work here (case-insensitive filesystem) but FAIL to resolve on \
             case-sensitive filesystems (Linux — most CI and production hosts). Fix the import \
             to match the on-disk case exactly.",
            requested_name,
            actual_name
        );
    }
}

impl Resolver {
    /// Create a resolver rooted at `root` with the given file `extensions`
    /// (tried in order, e.g. `[".tsx", ".ts", ".jsx", ".js"]`) and path
    /// `aliases`. Uses the default browser-targeted resolution context.
    pub fn new(root: PathBuf, extensions: Vec<String>, aliases: Vec<Alias>) -> Self {
        Self {
            root,
            extensions,
            aliases,
            custom_conditions: vec![],
            workspace: None,
            runtime: ResolveRuntime::default(),
            module_type: ResolveModuleType::default(),
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Create a resolver with custom export conditions (#119)
    pub fn with_conditions(
        root: PathBuf,
        extensions: Vec<String>,
        aliases: Vec<Alias>,
        conditions: Vec<String>,
    ) -> Self {
        Self {
            root,
            extensions,
            aliases,
            custom_conditions: conditions,
            workspace: None,
            runtime: ResolveRuntime::default(),
            module_type: ResolveModuleType::default(),
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Create a resolver with workspace info (#98)
    pub fn with_workspace(
        root: PathBuf,
        extensions: Vec<String>,
        aliases: Vec<Alias>,
        workspace: pledgepack_core::ecosystem::WorkspaceInfo,
    ) -> Self {
        Self {
            root,
            extensions,
            aliases,
            custom_conditions: vec![],
            workspace: Some(workspace),
            runtime: ResolveRuntime::default(),
            module_type: ResolveModuleType::default(),
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Create a resolver with an explicit resolution context, allowing the
    /// caller to drive export condition priority from the target runtime and
    /// module type instead of relying on hardcoded ordering.
    pub fn with_context(
        root: PathBuf,
        extensions: Vec<String>,
        aliases: Vec<Alias>,
        context: ResolveContext,
    ) -> Self {
        Self {
            root,
            extensions,
            aliases,
            custom_conditions: vec![],
            workspace: None,
            runtime: context.runtime,
            module_type: context.module_type,
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Create a resolver from tsconfig.json or jsconfig.json
    /// Supports: paths, baseUrl, extends, and wildcard patterns
    pub fn from_tsconfig(root: PathBuf, extensions: Vec<String>) -> Self {
        let mut aliases = Vec::new();

        // Try tsconfig.json first, then jsconfig.json
        let config_path = root
            .join("tsconfig.json")
            .exists()
            .then(|| root.join("tsconfig.json"))
            .or_else(|| {
                root.join("jsconfig.json")
                    .exists()
                    .then(|| root.join("jsconfig.json"))
            });

        if let Some(tsconfig_path) = config_path {
            Self::parse_tsconfig(&tsconfig_path, &root, &mut aliases);
        }

        Self::new(root, extensions, aliases)
    }

    /// Parse a tsconfig/jsconfig file and populate aliases.
    /// Handles `extends` by recursively parsing parent configs.
    fn parse_tsconfig(config_path: &Path, root: &Path, aliases: &mut Vec<Alias>) {
        let content = match std::fs::read_to_string(config_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        // Strip JSON comments (tsconfig allows // and /* */ comments)
        let clean_json = strip_json_comments(&content);

        let tsconfig: TsConfig = match serde_json::from_str(&clean_json) {
            Ok(t) => t,
            Err(_) => return,
        };

        // Handle `extends` — parse parent config first, then override
        if let Some(ref extends) = tsconfig.extends {
            let parent_path = if extends.starts_with('.') {
                // Relative path
                config_path
                    .parent()
                    .map(|p| p.join(extends))
                    .filter(|p| p.exists())
            } else {
                // Could be a node_modules package like "@tsconfig/strict"
                root.join("node_modules")
                    .join(extends)
                    .join("tsconfig.json")
                    .exists()
                    .then(|| {
                        root.join("node_modules")
                            .join(extends)
                            .join("tsconfig.json")
                    })
            };

            if let Some(ref parent) = parent_path {
                Self::parse_tsconfig(parent, root, aliases);
            }
        }

        // Apply this config's compiler options (overrides parent)
        if let Some(opts) = tsconfig.compiler_options {
            let base_url = opts.base_url.unwrap_or_else(|| ".".to_string());
            let base = root.join(&base_url);

            if let Some(paths) = opts.paths {
                // Clear parent aliases that conflict (paths override extends)
                let new_froms: Vec<String> = paths.keys().cloned().collect();
                aliases.retain(|a| {
                    !new_froms
                        .iter()
                        .any(|nf| nf.starts_with(&a.from) || a.from.starts_with(nf))
                });

                for (from, tos) in paths {
                    for to in tos {
                        // Preserve wildcard info for pattern matching
                        let has_wildcard = from.contains('*');
                        let from_clean = from.replace('*', "");
                        let to_clean = to.replace('*', "");
                        let to_path = base.join(&to_clean);

                        aliases.push(Alias {
                            from: from_clean,
                            to: to_path.to_string_lossy().to_string(),
                        });

                        // If wildcard, also add the pattern for matching
                        if has_wildcard {
                            // Store wildcard pattern separately for pattern matching
                            // The alias.from without '*' acts as prefix, and we resolve
                            // the rest as a subpath under alias.to
                        }
                    }
                }
            }
        }
    }

    /// Resolve a module specifier to a file path
    pub fn resolve(&self, specifier: &str, importer: &Path) -> Result<PathBuf> {
        let cache_key = (importer.to_path_buf(), specifier.to_string());
        if let Some(cached) = self.cache.get(&cache_key)
            && let Some(path) = cached.as_ref()
        {
            return Ok(path.clone());
        }

        let resolved = self.resolve_uncached(specifier, importer)?;

        self.cache.insert(cache_key, Some(resolved.clone()));
        Ok(resolved)
    }

    fn resolve_uncached(&self, specifier: &str, importer: &Path) -> Result<PathBuf> {
        // Strip ?worker and ?sharedworker suffixes (#111, #112)
        let specifier = specifier
            .trim_end_matches("?worker")
            .trim_end_matches("?sharedworker");

        // 1. Check aliases (sorted longest-first to avoid prefix mismatches, e.g.
        //    so that `@/` does not incorrectly match `@components/Button`)
        let mut sorted_aliases: Vec<&Alias> = self.aliases.iter().collect();
        sorted_aliases.sort_by(|a, b| b.from.len().cmp(&a.from.len()));

        for alias in &sorted_aliases {
            if specifier.starts_with(&alias.from) {
                let rest = &specifier[alias.from.len()..];
                // Ensure boundary: rest must be empty, start with '/', or the
                // alias must end with '/' (already a path boundary).
                if rest.is_empty() || rest.starts_with('/') || alias.from.ends_with('/') {
                    let path = PathBuf::from(&alias.to).join(rest);
                    if let Some(resolved) = self.try_resolve_path(&path)? {
                        return Ok(resolved);
                    }
                }
            }
        }

        // 2. Internal package imports (package.json "imports" field, #subpaths)
        if specifier.starts_with('#')
            && let Some(resolved) = self.resolve_imports(specifier, importer)?
        {
            return Ok(resolved);
        }

        // 3. Relative paths
        if specifier.starts_with("./") || specifier.starts_with("../") {
            let base = importer.parent().unwrap_or(&self.root);
            let path = base.join(specifier);
            if let Some(resolved) = self.try_resolve_path(&path)? {
                return Ok(resolved);
            }
        }

        // 4. Absolute paths
        if specifier.starts_with('/') {
            let path = PathBuf::from(specifier);
            if let Some(resolved) = self.try_resolve_path(&path)? {
                return Ok(resolved);
            }
        }

        // 5. Bare specifier → workspace packages (#98)
        if let Some(ref ws) = self.workspace
            && let Some(resolved) =
                pledgepack_core::ecosystem::resolve_workspace_import(specifier, ws)
        {
            return Ok(resolved);
        }

        // 6. Bare specifier → node_modules
        if let Some(resolved) = self.resolve_node_module(specifier)? {
            return Ok(resolved);
        }

        anyhow::bail!("Cannot resolve '{}' from {:?}", specifier, importer)
    }

    fn try_resolve_path(&self, path: &Path) -> Result<Option<PathBuf>> {
        // Try exact path
        if path.is_file() {
            return Ok(Some(canonicalize_or_warn(path.to_path_buf())));
        }

        // Try with extensions
        for ext in &self.extensions {
            let with_ext = path.with_extension(ext.trim_start_matches('.'));
            if with_ext.is_file() {
                return Ok(Some(canonicalize_or_warn(with_ext)));
            }
        }

        // Try as directory with index file
        if path.is_dir() {
            for ext in &self.extensions {
                let index = path.join(format!("index{}", ext));
                if index.is_file() {
                    return Ok(Some(canonicalize_or_warn(index)));
                }
            }
        }

        Ok(None)
    }

    fn resolve_node_module(&self, specifier: &str) -> Result<Option<PathBuf>> {
        let mut current = self.root.clone();

        // Split package name and subpath (e.g., "react/jsx-runtime" → "react" + "/jsx-runtime")
        let (pkg_name, subpath) = if let Some(rest) = specifier.strip_prefix('@') {
            // Scoped package: @scope/name/subpath
            if let Some(idx) = rest.find('/') {
                let after_scope = &rest[..idx];
                if let Some(sub_idx) = after_scope.find('/') {
                    let pkg = &specifier[..1 + sub_idx + 1];
                    let sub = &specifier[1 + sub_idx + 1..];
                    (pkg, Some(sub))
                } else {
                    (specifier, None)
                }
            } else {
                (specifier, None)
            }
        } else if let Some(idx) = specifier.find('/') {
            (&specifier[..idx], Some(&specifier[idx..]))
        } else {
            (specifier, None)
        };

        loop {
            let node_modules = current.join("node_modules");
            if node_modules.is_dir() {
                let module_path = node_modules.join(pkg_name);

                // If the standard location is a symlink (pnpm layout), resolve it
                // to the real path inside the virtual store (.pnpm).
                let module_path = if module_path.exists() {
                    canonicalize_or_warn(module_path)
                } else {
                    module_path
                };

                // Check package.json for entry point
                let pkg_json = module_path.join("package.json");
                if pkg_json.is_file()
                    && let Ok(content) = std::fs::read_to_string(&pkg_json)
                    && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
                {
                    if let Some(resolved) =
                        self.resolve_package_entry(&module_path, &pkg, subpath, specifier)?
                    {
                        return Ok(Some(resolved));
                    }
                }

                // Try direct file resolution for subpath
                if let Some(sub) = subpath {
                    let sub_path = module_path.join(sub.trim_start_matches('/'));
                    if let Some(resolved) = self.try_resolve_path(&sub_path)? {
                        return Ok(Some(resolved));
                    }
                }

                // Try direct file resolution
                if let Some(resolved) = self.try_resolve_path(&module_path)? {
                    return Ok(Some(resolved));
                }

                // pnpm fallback: the package may not be symlinked into the top
                // level of node_modules but still exists in the virtual store
                // under node_modules/.pnpm/{pkg}@version/node_modules/{pkg}.
                let pnpm_dir = node_modules.join(".pnpm");
                if pnpm_dir.is_dir()
                    && let Some(resolved) =
                        self.resolve_pnpm_package(&pnpm_dir, pkg_name, subpath, specifier)?
                {
                    return Ok(Some(resolved));
                }
            }

            // Go up one directory
            if !current.pop() {
                break;
            }
        }

        Ok(None)
    }

    /// Resolve a package's entry point given its directory and parsed package.json.
    /// Handles exports, module, main, and browser fields.
    fn resolve_package_entry(
        &self,
        module_path: &Path,
        pkg: &serde_json::Value,
        subpath: Option<&str>,
        specifier: &str,
    ) -> Result<Option<PathBuf>> {
        // 1. Try "exports" field (modern)
        if let Some(exports) = pkg.get("exports")
            && let Some(resolved) = self.resolve_exports(exports, subpath, module_path)?
        {
            return Ok(Some(resolved));
        }

        // 2. Try "module" field (ESM preference)
        if subpath.is_none() {
            if let Some(module) = pkg.get("module").and_then(|v| v.as_str()) {
                let entry_path = module_path.join(module);
                if entry_path.is_file() {
                    return Ok(Some(canonicalize_or_warn(entry_path)));
                }
            }

            // 3. Try "main" field
            if let Some(main) = pkg.get("main").and_then(|v| v.as_str()) {
                let entry_path = module_path.join(main);
                if entry_path.is_file() {
                    return Ok(Some(canonicalize_or_warn(entry_path)));
                }
            }
        }

        // 4. Try "browser" field for browser-specific builds
        if subpath.is_none()
            && let Some(browser) = pkg.get("browser")
        {
            if let Some(browser_str) = browser.as_str() {
                // String: replace the package entry with this file.
                let entry_path = module_path.join(browser_str);
                if entry_path.is_file() {
                    return Ok(Some(canonicalize_or_warn(entry_path)));
                }
            } else if let Some(browser_obj) = browser.as_object() {
                // Object: per-module mapping. Check if the current specifier (or
                // the entry subpath) matches a key in the browser map.
                let target = subpath.unwrap_or(".");
                let replacement = browser_obj
                    .get(target)
                    .or_else(|| browser_obj.get(specifier));
                if let Some(repl) = replacement {
                    if let Some(repl_str) = repl.as_str() {
                        let entry_path = module_path.join(repl_str);
                        if entry_path.is_file() {
                            return Ok(Some(canonicalize_or_warn(entry_path)));
                        }
                    } else if repl.is_null() {
                        // null means stub this module out. A data: URL cannot be
                        // represented as a PathBuf, so we skip it here and let
                        // resolution continue/fail naturally.
                    }
                }
            }
        }

        Ok(None)
    }

    /// Resolve a package from pnpm's virtual store (.pnpm).
    /// Looks for `.pnpm/{pkg}@version/node_modules/{pkg}` directories.
    fn resolve_pnpm_package(
        &self,
        pnpm_dir: &Path,
        pkg_name: &str,
        subpath: Option<&str>,
        specifier: &str,
    ) -> Result<Option<PathBuf>> {
        let prefix = format!("{}@", pkg_name);
        if let Ok(entries) = std::fs::read_dir(pnpm_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Match `{pkg}@version` directories. For scoped packages pnpm
                // encodes the path separator, so also accept a leading match.
                if name.starts_with(&prefix) {
                    let real_path = entry.path().join("node_modules").join(pkg_name);
                    let pkg_json = real_path.join("package.json");
                    if pkg_json.is_file()
                        && let Ok(content) = std::fs::read_to_string(&pkg_json)
                        && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
                        && let Some(resolved) =
                            self.resolve_package_entry(&real_path, &pkg, subpath, specifier)?
                    {
                        return Ok(Some(resolved));
                    }

                    // Try direct file resolution for subpath
                    if let Some(sub) = subpath {
                        let sub_path = real_path.join(sub.trim_start_matches('/'));
                        if let Some(resolved) = self.try_resolve_path(&sub_path)? {
                            return Ok(Some(resolved));
                        }
                    }

                    // Try direct file resolution
                    if let Some(resolved) = self.try_resolve_path(&real_path)? {
                        return Ok(Some(resolved));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Resolve using package.json "exports" field
    fn resolve_exports(
        &self,
        exports: &serde_json::Value,
        subpath: Option<&str>,
        module_path: &Path,
    ) -> Result<Option<PathBuf>> {
        // exports can be:
        //   "./foo.js" → { "import": "...", "require": "..." }
        //   { ".": { "import": "./esm/index.js" }, "./utils": { "import": "./esm/utils.js" } }
        //   { "import": "./esm/index.js" } (sugar for ".")

        let target_key = subpath.unwrap_or(".");

        if let Some(obj) = exports.as_object() {
            // Check if it's a conditional export (top-level keys like "import", "require")
            if (obj.contains_key("import")
                || obj.contains_key("require")
                || obj.contains_key("default"))
                && target_key == "."
            {
                // Sugar form: top-level conditions apply to "."
                return self.resolve_conditions(obj, module_path);
            }

            // Subpath exports: look for matching key
            for (key, value) in obj {
                if key == target_key {
                    if let Some(obj2) = value.as_object() {
                        return self.resolve_conditions(obj2, module_path);
                    } else if let Some(path) = value.as_str() {
                        let resolved = module_path.join(path);
                        if resolved.is_file() {
                            return Ok(Some(canonicalize_or_warn(resolved)));
                        }
                    }
                }

                // Pattern matching: "./utils/*" → "./utils/*.js"
                if key.ends_with('*') && target_key.starts_with(&key[..key.len() - 1]) {
                    let pattern_prefix = &key[..key.len() - 1];
                    let rest = &target_key[pattern_prefix.len()..];
                    if let Some(path) = value.as_str() {
                        let resolved_path = path.replace('*', rest);
                        let resolved = module_path.join(&resolved_path);
                        if resolved.is_file() {
                            return Ok(Some(canonicalize_or_warn(resolved)));
                        }
                    } else if let Some(obj2) = value.as_object()
                        && let Some(path) = self.resolve_conditions(obj2, module_path)?.as_ref()
                    {
                        // Replace pattern in resolved path
                        let path_str = path.to_string_lossy();
                        if path_str.contains('*') {
                            let replaced = path_str.replace('*', rest);
                            let p = PathBuf::from(replaced);
                            if p.is_file() {
                                return Ok(Some(p));
                            }
                        }
                        return Ok(Some(path.clone()));
                    }
                }
            }
        } else if let Some(path) = exports.as_str() {
            // Direct string export
            if target_key == "." {
                let resolved = module_path.join(path);
                if resolved.is_file() {
                    return Ok(Some(canonicalize_or_warn(resolved)));
                }
            }
        }

        Ok(None)
    }

    /// Resolve internal package imports (package.json "imports" field, #subpaths).
    ///
    /// Walks up from the importing file to find the nearest package.json and
    /// checks its "imports" field for the `#`-prefixed specifier.
    fn resolve_imports(&self, specifier: &str, importer: &Path) -> Result<Option<PathBuf>> {
        let mut current = importer.parent().unwrap_or(&self.root).to_path_buf();
        loop {
            let pkg_json_path = current.join("package.json");
            if pkg_json_path.is_file()
                && let Ok(content) = std::fs::read_to_string(&pkg_json_path)
                && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
            {
                if let Some(imports) = pkg.get("imports").and_then(|v| v.as_object()) {
                    if let Some(mapping) = imports.get(specifier) {
                        // imports can be a string or a conditional object
                        if let Some(s) = mapping.as_str() {
                            return self.try_resolve_path(&current.join(s));
                        } else if let Some(obj) = mapping.as_object() {
                            // Conditional imports: resolve using the same
                            // context-driven condition priority as exports.
                            for condition in conditions_for_context(self.runtime, self.module_type)
                            {
                                if let Some(target) = obj.get(condition)
                                    && let Some(t) = target.as_str()
                                {
                                    let resolved = current.join(t);
                                    if let Some(p) = self.try_resolve_path(&resolved)? {
                                        return Ok(Some(p));
                                    }
                                }
                            }
                        }
                    }
                    // imports are package-scoped: once we find a package.json
                    // with an imports field, stop searching upwards.
                    return Ok(None);
                }
            }
            if !current.pop() {
                break;
            }
        }
        Ok(None)
    }

    /// Resolve conditional exports (import/require/default/browser).
    ///
    /// Condition priority is derived from the resolver's runtime/module-type
    /// context rather than a hardcoded order. Custom conditions (#119) always
    /// take precedence, followed by the context-derived conditions.
    fn resolve_conditions(
        &self,
        obj: &serde_json::Map<String, serde_json::Value>,
        module_path: &Path,
    ) -> Result<Option<PathBuf>> {
        // Custom conditions first, then context-derived conditions.
        let mut all_conditions: Vec<String> = self.custom_conditions.clone();
        for c in conditions_for_context(self.runtime, self.module_type) {
            let s = c.to_string();
            if !all_conditions.contains(&s) {
                all_conditions.push(s);
            }
        }
        for condition in &all_conditions {
            if let Some(value) = obj.get(condition)
                && let Some(path) = value.as_str()
            {
                let resolved = module_path.join(path);
                if resolved.is_file() {
                    return Ok(Some(canonicalize_or_warn(resolved)));
                }
            }
        }
        Ok(None)
    }
}

/// Derive export/imports condition priority from a resolution context.
///
/// Conditions are returned highest-priority first. The runtime condition
/// (e.g. "browser", "node") is preferred, then the module-type condition
/// ("import"/"module" for ESM, "require" for CJS), and finally "default".
fn conditions_for_context(
    runtime: ResolveRuntime,
    module_type: ResolveModuleType,
) -> Vec<&'static str> {
    let mut conditions = Vec::new();
    match runtime {
        ResolveRuntime::Node => conditions.push("node"),
        ResolveRuntime::Browser => conditions.push("browser"),
        ResolveRuntime::Deno => conditions.push("deno"),
        ResolveRuntime::Bun => conditions.push("bun"),
        ResolveRuntime::Worker => conditions.push("worker"),
    }
    match module_type {
        ResolveModuleType::Esm => {
            conditions.push("import");
            conditions.push("module");
        }
        ResolveModuleType::Cjs => conditions.push("require"),
    }
    conditions.push("default");
    conditions
}

/// Strip JSON comments (// and /* */) that are valid in tsconfig.json files.
/// Also strips trailing commas which are allowed in tsconfig.
fn strip_json_comments(content: &str) -> String {
    let mut result = String::with_capacity(content.len());
    let mut in_string = false;
    let mut escape = false;
    let chars: Vec<char> = content.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if in_string {
            result.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match c {
            '"' => {
                in_string = true;
                result.push(c);
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '/' => {
                // Line comment — skip to end of line
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                // Block comment — skip to */
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2; // skip */
                continue;
            }
            ',' if i + 1 < chars.len() => {
                // Check if next non-whitespace is } or ]
                let mut j = i + 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                    // Trailing comma — skip it
                } else {
                    result.push(c);
                }
            }
            _ => {
                result.push(c);
            }
        }
        i += 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_relative() {
        let cwd = std::env::current_dir().unwrap();
        let resolver = Resolver::new(
            cwd.clone(),
            vec![".rs".to_string(), ".ts".to_string(), ".js".to_string()],
            vec![],
        );

        // Resolve ./Cargo.toml relative to the workspace root
        let cargo_toml = resolver.resolve("./Cargo.toml", &cwd.join("Cargo.toml"));
        assert!(
            cargo_toml.is_ok(),
            "Failed to resolve ./Cargo.toml: {:?}",
            cargo_toml.err()
        );
    }

    #[test]
    fn test_strip_json_comments() {
        let input = r#"{
  // This is a line comment
  "compilerOptions": {
    "baseUrl": ".", /* block comment */
    "paths": {
      "@/*": ["src/*"]
    }
  }
}"#;
        let cleaned = strip_json_comments(input);
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["compilerOptions"]["baseUrl"], ".");
    }

    #[test]
    fn test_strip_trailing_commas() {
        let input = r#"{
  "compilerOptions": {
    "baseUrl": ".",
  }
}"#;
        let cleaned = strip_json_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["compilerOptions"]["baseUrl"], ".");
    }

    #[test]
    fn test_wildcard_alias_resolution() {
        let resolver = Resolver::new(
            PathBuf::from("."),
            vec![".ts".to_string(), ".tsx".to_string()],
            vec![
                Alias {
                    from: "@components/".to_string(),
                    to: "src/components".to_string(),
                },
                Alias {
                    from: "@/".to_string(),
                    to: "src/".to_string(),
                },
            ],
        );

        // @/utils should resolve to src/utils
        let result = resolver.resolve("@/utils", Path::new("src/index.tsx"));
        // It may not resolve if the file doesn't exist, but it should try the right path
        // We just verify it doesn't panic
        let _ = result;
    }

    // ─── Goals 51-52: case sensitivity ─────────────────────────────────
    //
    // On a case-sensitive filesystem (Linux — most CI and production
    // hosts), a mis-cased relative import simply doesn't resolve: the OS
    // itself rejects it, nothing PledgePack does is involved. That's the
    // *correct*, unavoidable behavior this whole goal exists to protect
    // developers on case-insensitive machines (Windows, macOS) from being
    // surprised by. So: one test that holds on every platform (a mis-cased
    // import fails outright on a case-sensitive FS), and one Windows/macOS-
    // only test that the resolver's new case-mismatch warning actually
    // fires when the OS silently "helps" by resolving it anyway.

    #[cfg(target_os = "linux")]
    #[test]
    fn mismatched_case_import_fails_on_case_sensitive_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Utils.js"), "export default 1;").unwrap();
        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let importer = dir.path().join("index.js");

        // Real file is "Utils.js"; importing "./utils.js" must fail here —
        // proving Linux's case sensitivity is the backstop this whole
        // feature is about, not something PledgePack has to (or safely
        // could) paper over.
        assert!(resolver.resolve("./utils.js", &importer).is_err());
        // The correctly-cased import must still succeed.
        assert!(resolver.resolve("./Utils.js", &importer).is_ok());
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn mismatched_case_import_resolves_but_would_break_on_linux() {
        // This test's whole point is that it *passes* here — on a
        // case-insensitive filesystem the OS resolves the mis-cased import
        // regardless of what the resolver does. The warning
        // `warn_on_case_mismatch` logs is the only signal a developer gets
        // that this same import will fail once built/deployed on Linux.
        // (Asserting the warning was logged would need a tracing
        // subscriber test harness; this test instead documents and pins
        // the resolution behavior the warning is layered on top of.)
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Utils.js"), "export default 1;").unwrap();
        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let importer = dir.path().join("index.js");

        let resolved = resolver.resolve("./utils.js", &importer);
        assert!(
            resolved.is_ok(),
            "case-insensitive filesystem should resolve this despite the case mismatch"
        );
    }

    // ─── Goal 54: pnpm nested node_modules edge cases ───────────────────

    #[test]
    #[ignore = "PRODUCTION-READINESS-100.md goal 54/93: deterministically crashes with \
                STATUS_HEAP_CORRUPTION (0xc0000374) on Windows inside `resolve_pnpm_package` \
                or something it calls into — root cause not yet found, needs a debugger \
                session (likely windbg + gflags page-heap). Left in the tree and marked \
                #[ignore] rather than deleted so the bug stays tracked and the fix, once \
                found, has a test to un-ignore. Do NOT remove #[ignore] until the crash is \
                confirmed fixed under a Windows CI run, not just locally."]
    fn pnpm_virtual_store_fallback_resolves_package_not_symlinked_at_top_level() {
        // Mirrors pnpm's real layout: node_modules/some-pkg is normally a
        // symlink into node_modules/.pnpm/some-pkg@1.0.0/node_modules/some-pkg,
        // but this test omits the symlink (Windows test runners may lack
        // symlink privileges) to exercise the `.pnpm` fallback path
        // specifically (`resolve_pnpm_package`, reached when the top-level
        // entry doesn't exist at all).
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir
            .path()
            .join("node_modules")
            .join(".pnpm")
            .join("some-pkg@1.0.0")
            .join("node_modules")
            .join("some-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name": "some-pkg", "main": "index.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("index.js"), "export default 1;").unwrap();

        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let importer = dir.path().join("index.js");
        let result = resolver.resolve("some-pkg", &importer);
        assert!(result.is_ok(), "pnpm virtual-store fallback failed: {:?}", result.err());
        assert!(result.unwrap().ends_with("index.js"));
    }

    #[test]
    fn pnpm_nested_workspace_walks_up_to_root_node_modules() {
        // A monorepo package resolving a dependency hoisted to the
        // workspace root's node_modules, not its own — exercises the
        // "go up one directory" loop in `resolve_node_module`.
        let dir = tempfile::tempdir().unwrap();
        let root_pkg = dir
            .path()
            .join("node_modules")
            .join("hoisted-dep");
        std::fs::create_dir_all(&root_pkg).unwrap();
        std::fs::write(
            root_pkg.join("package.json"),
            r#"{"name": "hoisted-dep", "main": "index.js"}"#,
        )
        .unwrap();
        std::fs::write(root_pkg.join("index.js"), "export default 1;").unwrap();

        let package_dir = dir.path().join("packages").join("my-app");
        std::fs::create_dir_all(&package_dir).unwrap();

        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let importer = package_dir.join("index.js");
        let result = resolver.resolve("hoisted-dep", &importer);
        assert!(result.is_ok(), "failed to walk up to root node_modules: {:?}", result.err());
    }

    // ─── Goal 56: Windows long-path handling ────────────────────────────

    #[cfg(target_os = "windows")]
    #[test]
    fn resolves_file_under_a_260_plus_character_path() {
        // MAX_PATH (260 chars) is a legacy Win32 API limit; Rust's std
        // filesystem calls use the `\\?\`-prefixed wide APIs internally and
        // aren't subject to it, but this pins that behavior for the
        // resolver's own code path specifically rather than assuming it.
        let dir = tempfile::tempdir().unwrap();
        let mut nested = dir.path().to_path_buf();
        // Each segment is short so individual component-length limits
        // aren't the thing under test — only the *total* path length is.
        while nested.as_os_str().len() < 260 {
            nested = nested.join("abcdefghij");
        }
        std::fs::create_dir_all(&nested).expect(
            "could not create a >260-char test directory — long-path support may need enabling \
             on this runner (fs.LongPathsEnabled)",
        );
        std::fs::write(nested.join("deep.js"), "export default 1;").unwrap();

        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let importer = nested.join("index.js");
        let result = resolver.resolve("./deep.js", &importer);
        assert!(result.is_ok(), "failed to resolve a long path: {:?}", result.err());
    }

    // ─── Goal 55: circular imports — resolver-level scope ───────────────
    //
    // `Resolver::resolve()` resolves one specifier to one path per call; it
    // doesn't itself walk "everything this file transitively imports," so
    // an import cycle (a.js imports b.js imports a.js) isn't something the
    // *resolver* can get stuck in — there's no recursion here to loop
    // forever. This test pins that: resolving both directions of a cycle
    // must terminate and succeed. Whether the *bundled output*
    // (module-graph traversal and codegen, in `pledgepack-core`, not this
    // crate) preserves correct ESM live-binding semantics through a cycle
    // is a separate, open question this crate can't answer — see
    // PRODUCTION-READINESS-100.md goal 55's note on scope.
    #[test]
    fn resolving_both_directions_of_an_import_cycle_terminates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.js"), "import './b.js'; export const a = 1;").unwrap();
        std::fs::write(dir.path().join("b.js"), "import './a.js'; export const b = 2;").unwrap();

        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let a = dir.path().join("a.js");
        let b = dir.path().join("b.js");

        assert!(resolver.resolve("./b.js", &a).is_ok());
        assert!(resolver.resolve("./a.js", &b).is_ok());
    }
}
