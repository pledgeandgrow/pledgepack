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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `package.json` `exports`/`imports` map matching — pure matching logic,
/// shared with `pledgepack-core` (which re-exports this module).
pub mod package_map;

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
    /// Optional workspace packages for monorepo resolution (#98), keyed by
    /// package name (e.g. `"@acme/ui"` → its workspace package).
    workspace: Option<HashMap<String, WorkspacePackage>>,
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

/// A workspace (monorepo) package as the resolver needs it: the on-disk
/// package directory plus its package.json entry points. This is a subset of
/// `pledgepack_core::ecosystem::WorkspacePackage` — keeping it here is what
/// lets the resolver be a leaf crate the engine can depend on.
#[derive(Debug, Clone, Default)]
pub struct WorkspacePackage {
    /// Package directory (contains `package.json`).
    pub path: PathBuf,
    /// `main` field, if any.
    pub main: Option<String>,
    /// `module` field, if any.
    pub module: Option<String>,
    /// `exports` field, if any.
    pub exports: Option<serde_json::Value>,
}

/// Resolve a bare specifier against workspace packages: package `exports` for
/// subpaths, `module`/`main`/index files for the package root, then direct
/// file + extension probing. Returns the resolved file path when found.
///
/// `extensions` are probed in order for extensionless subpaths
/// (e.g. `[".ts", ".tsx", ".js", ".jsx", ".mjs", ".json"]`).
pub fn resolve_workspace_import(
    specifier: &str,
    packages: &HashMap<String, WorkspacePackage>,
    extensions: &[String],
) -> Option<PathBuf> {
    // Scoped names are two path segments: "@scope/name[/sub…]".
    let (pkg_name, subpath) = if let Some(rest) = specifier.strip_prefix('@') {
        match rest.find('/') {
            Some(first) => match rest[first + 1..].find('/') {
                Some(second) => (
                    &specifier[..first + second + 2],
                    Some(&rest[first + second + 2..]),
                ),
                None => (specifier, None),
            },
            None => (specifier, None),
        }
    } else if let Some(pos) = specifier.find('/') {
        (&specifier[..pos], Some(&specifier[pos + 1..]))
    } else {
        (specifier, None)
    };

    let pkg = packages.get(pkg_name)?;
    if let Some(sub) = subpath {
        // `exports` encapsulates the package: an unlisted subpath must not
        // probe files directly (Node's PACKAGE_PATH_NOT_EXPORTED).
        if let Some(exports) = &pkg.exports {
            if let Some(obj) = exports.as_object() {
                let key = format!("./{}", sub);
                if let Some(v) = obj.get(&key).and_then(|v| v.as_str()) {
                    let p = pkg.path.join(v);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
            return None;
        }
        let direct = pkg.path.join(sub);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in extensions {
            let ext = ext.trim_start_matches('.');
            let p = direct.with_extension(ext);
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
#[serde(rename_all = "camelCase")]
struct TsConfig {
    compiler_options: Option<CompilerOptions>,
    extends: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
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

/// One lookup in a `browser` object map: string values are package-relative
/// replacement file paths; `false`/`null` means "stub this module out",
/// which a file-path resolver cannot represent — it is skipped so resolution
/// falls through to the ordinary file (no worse than having no map at all).
fn browser_map_lookup(
    browser_obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    module_path: &Path,
) -> Result<Option<PathBuf>> {
    match browser_obj.get(key).and_then(|v| v.as_str()) {
        Some(target) => {
            let p = module_path.join(target.trim_start_matches("./"));
            Ok(p.is_file().then(|| canonicalize_or_warn(p)))
        }
        None => Ok(None),
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

    /// Create a resolver with workspace packages for monorepo resolution (#98).
    /// `workspace` maps package names to their [`WorkspacePackage`]; build it
    /// from `pledgepack_core::ecosystem::detect_workspace` (or any scan) — see
    /// `WorkspaceInfo::resolver_packages` in core.
    pub fn with_workspace(
        root: PathBuf,
        extensions: Vec<String>,
        aliases: Vec<Alias>,
        workspace: HashMap<String, WorkspacePackage>,
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

    /// Attach workspace packages to an existing resolver — for callers that
    /// constructed it via `with_conditions`/`with_context` and only learn the
    /// workspace map later.
    pub fn set_workspace(&mut self, workspace: HashMap<String, WorkspacePackage>) {
        self.workspace = Some(workspace);
    }

    /// Set the resolution context (target runtime + importer module type) on
    /// an existing resolver — for callers that built it via
    /// `with_conditions`/`with_workspace` and need to drive condition
    /// priority afterwards.
    pub fn set_context(&mut self, context: ResolveContext) {
        self.runtime = context.runtime;
        self.module_type = context.module_type;
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

        // On Windows, specifiers may use backslashes (`.\foo`, `..\bar`,
        // `C:\proj\a.ts`); a bare package specifier never contains one.
        #[cfg(windows)]
        let normalized_specifier = specifier.replace('\\', "/");
        #[cfg(windows)]
        let specifier = normalized_specifier.as_str();

        // 1. Check aliases (sorted longest-first to avoid prefix mismatches, e.g.
        //    so that `@/` does not incorrectly match `@components/Button`)
        let mut sorted_aliases: Vec<&Alias> = self.aliases.iter().collect();
        sorted_aliases.sort_by_key(|a| std::cmp::Reverse(a.from.len()));

        for alias in &sorted_aliases {
            if specifier.starts_with(&alias.from) {
                let rest = &specifier[alias.from.len()..];
                // Ensure boundary: rest must be empty, start with '/', or the
                // alias must end with '/' (already a path boundary).
                if rest.is_empty() || rest.starts_with('/') || alias.from.ends_with('/') {
                    // `join("")` appends a trailing separator — a file path
                    // plus "/" is not a file. Exact alias hits use `to` as-is.
                    // `rest` keeps its leading `/` (`@/` → `/components/x`) —
                    // join() would treat that as a root and discard `to`, so
                    // strip it first.
                    let path = if rest.is_empty() {
                        PathBuf::from(&alias.to)
                    } else {
                        PathBuf::from(&alias.to).join(rest.trim_start_matches('/'))
                    };
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
        if specifier.starts_with("./")
            || specifier.starts_with("../")
            || specifier == "."
            || specifier == ".."
        {
            let base = importer.parent().unwrap_or(&self.root);
            let path = base.join(specifier);
            if let Some(resolved) = self.try_resolve_path(&path)? {
                return Ok(resolved);
            }
        }

        // 4. Absolute paths (POSIX `/x`, or a Windows drive path `C:/x`)
        if specifier.starts_with('/') || Path::new(specifier).is_absolute() {
            let path = PathBuf::from(specifier);
            if let Some(resolved) = self.try_resolve_path(&path)? {
                return Ok(resolved);
            }
        }

        // 5. Bare specifier → workspace packages (#98)
        if let Some(ref ws) = self.workspace
            && let Some(resolved) = resolve_workspace_import(specifier, ws, &self.extensions)
        {
            return Ok(resolved);
        }

        // 6. Bare specifier → node_modules
        if let Some(resolved) = self.resolve_node_module(specifier, importer)? {
            return Ok(resolved);
        }

        anyhow::bail!("Cannot resolve '{}' from {:?}", specifier, importer)
    }

    fn try_resolve_path(&self, path: &Path) -> Result<Option<PathBuf>> {
        // Try exact path
        if path.is_file() {
            return Ok(Some(canonicalize_or_warn(path.to_path_buf())));
        }

        // Try appending each extension: `./foo.config` -> `foo.config.ts`.
        // (Replacing the extension here used to collapse `./foo.config` and
        // `./jquery.min` onto an unrelated `foo.ts` / `jquery.ts`.)
        for ext in &self.extensions {
            let mut s = path.as_os_str().to_os_string();
            s.push(if ext.starts_with('.') {
                ext.clone()
            } else {
                format!(".{ext}")
            });
            let with_ext = PathBuf::from(s);
            if with_ext.is_file() {
                return Ok(Some(canonicalize_or_warn(with_ext)));
            }
        }

        // TypeScript ESM convention: `./util.js` may refer to `util.ts`.
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "js" | "jsx" | "mjs" | "cjs"))
        {
            for ext in &self.extensions {
                let with_ext = path.with_extension(ext.trim_start_matches('.'));
                if with_ext.is_file() {
                    return Ok(Some(canonicalize_or_warn(with_ext)));
                }
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

    fn resolve_node_module(&self, specifier: &str, importer: &Path) -> Result<Option<PathBuf>> {
        // Split package name and subpath (e.g., "react/jsx-runtime" → "react" + "/jsx-runtime")
        let (pkg_name, subpath) = if let Some(rest) = specifier.strip_prefix('@') {
            // Scoped package: @scope/name/subpath
            if let Some(idx) = rest.find('/') {
                // `idx` ends the scope; the package name runs to the next
                // '/', which starts the subpath. (This used to look for a
                // '/' inside the scope itself, so scoped subpath imports
                // like `@scope/pkg/feature` never split and bypassed the
                // package's `exports` map.)
                let after_scope = &rest[idx + 1..];
                if let Some(sub_idx) = after_scope.find('/') {
                    let split = 1 + idx + 1 + sub_idx;
                    (&specifier[..split], Some(&specifier[split..]))
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

        // A bare specifier must not climb out of its package directory
        // (`pkg/../../secret.js`), nor name `.`/`..`/backslash paths.
        if specifier
            .split('/')
            .any(|seg| seg == ".." || seg == "." || seg.contains('\\'))
            || pkg_name.is_empty()
        {
            return Ok(None);
        }

        // Node resolution starts at the *importer's* directory and walks up
        // (so a package's own `node_modules`, a pnpm virtual-store sibling
        // dependency, or a workspace package's local install wins), then
        // falls back to walking up from the project root.
        let mut starts: Vec<PathBuf> = Vec::with_capacity(2);
        if importer.is_absolute()
            && let Some(dir) = importer.parent()
        {
            starts.push(dir.to_path_buf());
        }
        starts.push(self.root.clone());

        let mut visited = std::collections::HashSet::new();
        for start in starts {
            let mut current = start;
            loop {
                let is_nm_dir = current
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("node_modules"));
                if !is_nm_dir
                    && visited.insert(current.clone())
                    && let Some(resolved) =
                        self.resolve_in_node_modules(&current, pkg_name, subpath, specifier)?
                {
                    return Ok(Some(resolved));
                }
                // Go up one directory
                if !current.pop() {
                    break;
                }
            }
        }

        Ok(None)
    }

    /// Look for `pkg_name` in `<dir>/node_modules` (following pnpm symlinks,
    /// then the `.pnpm` virtual store and its hoisted `.pnpm/node_modules`).
    fn resolve_in_node_modules(
        &self,
        dir: &Path,
        pkg_name: &str,
        subpath: Option<&str>,
        specifier: &str,
    ) -> Result<Option<PathBuf>> {
        let node_modules = dir.join("node_modules");
        if !node_modules.is_dir() {
            return Ok(None);
        }
        let module_path = node_modules.join(pkg_name);

        // If the standard location is a symlink (pnpm layout), resolve it
        // to the real path inside the virtual store (.pnpm).
        let module_path = if module_path.exists() {
            canonicalize_or_warn(module_path)
        } else {
            module_path
        };

        if let Some(resolved) = self.resolve_package_dir(&module_path, subpath, specifier)? {
            return Ok(Some(resolved));
        }

        // pnpm fallbacks: the package may not be symlinked into the top
        // level of node_modules but still exist in the virtual store
        // under node_modules/.pnpm/{pkg}@version/node_modules/{pkg}, or in
        // pnpm's hoisted dir node_modules/.pnpm/node_modules/{pkg}.
        let pnpm_dir = node_modules.join(".pnpm");
        if pnpm_dir.is_dir() {
            let hoisted = pnpm_dir.join("node_modules").join(pkg_name);
            if hoisted.exists()
                && let Some(resolved) =
                    self.resolve_package_dir(&canonicalize_or_warn(hoisted), subpath, specifier)?
            {
                return Ok(Some(resolved));
            }
            if let Some(resolved) =
                self.resolve_pnpm_package(&pnpm_dir, pkg_name, subpath, specifier)?
            {
                return Ok(Some(resolved));
            }
        }
        Ok(None)
    }

    /// Resolve `subpath` (or the entry point) inside the package directory
    /// `module_path`: package.json `exports`/`module`/`main`, then plain files.
    fn resolve_package_dir(
        &self,
        module_path: &Path,
        subpath: Option<&str>,
        specifier: &str,
    ) -> Result<Option<PathBuf>> {
        // Check package.json for entry point
        let pkg_json = module_path.join("package.json");
        if pkg_json.is_file()
            && let Ok(content) = std::fs::read_to_string(&pkg_json)
            && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
        {
            if let Some(resolved) =
                self.resolve_package_entry(module_path, &pkg, subpath, specifier)?
            {
                return Ok(Some(resolved));
            }
            // `exports` encapsulates the package: a subpath it doesn't list
            // is not reachable by direct-file probing (Node's
            // PACKAGE_PATH_NOT_EXPORTED).
            if pkg.get("exports").is_some() {
                return Ok(None);
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
        self.try_resolve_path(module_path)
    }

    /// Resolve a package's entry point given its directory and parsed package.json.
    /// Handles exports, module, main, and browser fields.
    fn resolve_package_entry(
        &self,
        module_path: &Path,
        pkg: &serde_json::Value,
        subpath: Option<&str>,
        _specifier: &str,
    ) -> Result<Option<PathBuf>> {
        // 1. Try "exports" field (modern)
        if let Some(exports) = pkg.get("exports")
            && let Some(resolved) = self.resolve_exports(exports, subpath, module_path)?
        {
            return Ok(Some(resolved));
        }

        // 2. "browser" field — browser builds only. The object form maps
        // *package-relative module paths* (`"./server.js": "./browser.js"`)
        // and wins over module/main for the keys it covers (webpack/
        // browserify semantics); the string form replaces the entry point.
        if self.runtime == ResolveRuntime::Browser
            && let Some(browser) = pkg.get("browser")
        {
            if let Some(browser_str) = browser.as_str() {
                if subpath.is_none() {
                    let entry_path = module_path.join(browser_str);
                    if entry_path.is_file() {
                        return Ok(Some(canonicalize_or_warn(entry_path)));
                    }
                }
            } else if let Some(browser_obj) = browser.as_object() {
                // Subpath forms: "pkg/lib/server" is looked up as
                // "./lib/server", "./lib/server.js", "./lib/server/index.js".
                if let Some(sub) = subpath {
                    let sub = sub.trim_start_matches('/');
                    for key in [
                        format!("./{sub}"),
                        format!("./{sub}.js"),
                        format!("./{sub}/index.js"),
                    ] {
                        if let Some(resolved) = browser_map_lookup(browser_obj, &key, module_path)?
                        {
                            return Ok(Some(resolved));
                        }
                    }
                }
                // Entry form: the map key is the *would-be* entry file
                // ("./main.js" → "./main.browser.js").
                if subpath.is_none()
                    && let Some(resolved) =
                        self.browser_entry_override(browser_obj, pkg, module_path)?
                {
                    return Ok(Some(resolved));
                }
            }
        }

        // 3. Try "module" field (ESM preference)
        if subpath.is_none() {
            if let Some(module) = pkg.get("module").and_then(|v| v.as_str()) {
                let entry_path = module_path.join(module);
                if entry_path.is_file() {
                    return Ok(Some(canonicalize_or_warn(entry_path)));
                }
            }

            // 4. Try "main" field
            if let Some(main) = pkg.get("main").and_then(|v| v.as_str()) {
                let entry_path = module_path.join(main);
                if entry_path.is_file() {
                    return Ok(Some(canonicalize_or_warn(entry_path)));
                }
            }
        }

        Ok(None)
    }

    /// Look up the entry file a package *would* resolve to (`module`, then
    /// `main`, then `index.js`) in a `browser` object map — the browserify
    /// form where the map key is the main field's path, e.g.
    /// `"browser": {"./lib/node.js": "./lib/browser.js"}`.
    fn browser_entry_override(
        &self,
        browser_obj: &serde_json::Map<String, serde_json::Value>,
        pkg: &serde_json::Value,
        module_path: &Path,
    ) -> Result<Option<PathBuf>> {
        let entry = pkg
            .get("module")
            .and_then(|v| v.as_str())
            .or_else(|| pkg.get("main").and_then(|v| v.as_str()))
            .unwrap_or("index.js")
            .trim_start_matches("./");
        for key in [format!("./{entry}"), entry.to_string()] {
            if let Some(resolved) = browser_map_lookup(browser_obj, &key, module_path)? {
                return Ok(Some(resolved));
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
        // pnpm encodes a scoped package's '/' as '+' in virtual-store
        // directory names (`@scope+name@1.0.0`).
        let prefix = format!("{}@", pkg_name.replace('/', "+"));
        if let Ok(entries) = std::fs::read_dir(pnpm_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Match `{pkg}@version` directories. For scoped packages pnpm
                // encodes the path separator, so also accept a leading match.
                if name.starts_with(&prefix) {
                    let real_path = entry.path().join("node_modules").join(pkg_name);
                    let pkg_json = real_path.join("package.json");
                    let pkg = if pkg_json.is_file() {
                        std::fs::read_to_string(&pkg_json)
                            .ok()
                            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                    } else {
                        None
                    };
                    if let Some(pkg) = &pkg {
                        if let Some(resolved) =
                            self.resolve_package_entry(&real_path, pkg, subpath, specifier)?
                        {
                            return Ok(Some(resolved));
                        }
                        // `exports` encapsulates the package — no direct-file
                        // probing for unlisted subpaths.
                        if pkg.get("exports").is_some() {
                            continue;
                        }
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

    /// Resolve using package.json "exports" field.
    ///
    /// Supports the string/array/conditions sugar forms, exact subpath keys
    /// (`"./feat"`), and subpath patterns with a single `*` anywhere in the
    /// key (`"./features/*.js"`), choosing the most specific matching key
    /// like Node does (longest prefix before the `*`).
    fn resolve_exports(
        &self,
        exports: &serde_json::Value,
        subpath: Option<&str>,
        module_path: &Path,
    ) -> Result<Option<PathBuf>> {
        // `subpath` arrives as "/feat" (the tail of `pkg/feat`) but `exports`
        // keys are "./feat" — without this every subpath export missed its
        // key and only worked when a same-named file happened to exist.
        let normalized_subpath = subpath.map(|s| {
            if s.starts_with("./") {
                s.to_string()
            } else {
                format!(
                    ".{}",
                    if s.starts_with('/') {
                        s.to_string()
                    } else {
                        format!("/{s}")
                    }
                )
            }
        });
        let target_key = normalized_subpath.as_deref().unwrap_or(".");

        let subpath_map = exports
            .as_object()
            .filter(|obj| obj.keys().any(|k| k.starts_with('.')));
        let Some(obj) = subpath_map else {
            // String / array / top-level conditions: sugar for the "." export.
            if target_key != "." {
                return Ok(None);
            }
            return self.resolve_target(exports, module_path, None, false, None);
        };

        if let Some(value) = obj.get(target_key)
            && !target_key.contains('*')
        {
            return self.resolve_target(value, module_path, None, false, None);
        }

        match best_pattern_match(obj.keys().map(String::as_str), target_key) {
            Some((key, star)) => {
                self.resolve_target(&obj[key], module_path, Some(&star), false, None)
            }
            None => Ok(None),
        }
    }

    /// Resolve an `exports`/`imports` target: a string path, an array of
    /// fallbacks, a conditions object (nested to any depth), or `null`.
    ///
    /// * `star` substitutes `*` in pattern targets.
    /// * `imports` targets (`allow_bare`) may also be bare package specifiers,
    ///   resolved from `importer`; `probe` lets `imports` targets use
    ///   extension/index probing like the pre-existing behaviour.
    fn resolve_target(
        &self,
        target: &serde_json::Value,
        base: &Path,
        star: Option<&str>,
        probe: bool,
        bare_from: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        match target {
            serde_json::Value::String(s) => {
                let s = match star {
                    Some(star) => s.replace('*', star),
                    None => s.clone(),
                };
                if let Some(rel) = s.strip_prefix("./") {
                    // Targets are package-relative: never `..`, never into a
                    // nested node_modules.
                    if rel
                        .split(['/', '\\'])
                        .any(|seg| seg == ".." || seg.eq_ignore_ascii_case("node_modules"))
                    {
                        return Ok(None);
                    }
                    let resolved = base.join(rel);
                    if probe {
                        return self.try_resolve_path(&resolved);
                    }
                    if resolved.is_file() {
                        return Ok(Some(canonicalize_or_warn(resolved)));
                    }
                    return Ok(None);
                }
                if let Some(importer) = bare_from
                    && !s.starts_with('/')
                    && !s.starts_with("../")
                    && !s.is_empty()
                {
                    return self.resolve_node_module(&s, importer);
                }
                Ok(None)
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(p) = self.resolve_target(item, base, star, probe, bare_from)? {
                        return Ok(Some(p));
                    }
                }
                Ok(None)
            }
            serde_json::Value::Object(obj) => {
                // Custom conditions first, then context-derived conditions.
                for condition in self.condition_list() {
                    if let Some(value) = obj.get(&condition)
                        && let Some(p) = self.resolve_target(value, base, star, probe, bare_from)?
                    {
                        return Ok(Some(p));
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Condition names in priority order: custom conditions (#119) first,
    /// then those derived from the runtime/module-type context.
    fn condition_list(&self) -> Vec<String> {
        let mut all_conditions: Vec<String> = self.custom_conditions.clone();
        for c in conditions_for_context(self.runtime, self.module_type) {
            let s = c.to_string();
            if !all_conditions.contains(&s) {
                all_conditions.push(s);
            }
        }
        all_conditions
    }

    /// Resolve internal package imports (package.json "imports" field, #subpaths).
    ///
    /// Walks up from the importing file to find the nearest package.json and
    /// checks its "imports" field for the `#`-prefixed specifier. Supports
    /// exact keys and `*` patterns (`"#utils/*": "./src/utils/*.js"`), nested
    /// conditions, and bare-package targets (`"#dep": "dep-pkg"`).
    fn resolve_imports(&self, specifier: &str, importer: &Path) -> Result<Option<PathBuf>> {
        let mut current = importer.parent().unwrap_or(&self.root).to_path_buf();
        loop {
            let pkg_json_path = current.join("package.json");
            if pkg_json_path.is_file()
                && let Ok(content) = std::fs::read_to_string(&pkg_json_path)
                && let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content)
            {
                // Node package-scope semantics: the *nearest* package.json
                // bounds the scope. If it has no `imports` field the `#`
                // specifier is unresolvable — we must NOT keep climbing to an
                // ancestor package.json that happens to have one.
                let Some(imports) = pkg.get("imports").and_then(|v| v.as_object()) else {
                    anyhow::bail!(
                        "package \"imports\" specifier '{specifier}' cannot be resolved: \
                         {} has no \"imports\" field",
                        pkg_json_path.display()
                    );
                };
                let hit = if let Some(mapping) = imports.get(specifier)
                    && !specifier.contains('*')
                {
                    Some((mapping, None))
                } else {
                    best_pattern_match(imports.keys().map(String::as_str), specifier)
                        .map(|(key, star)| (&imports[key], Some(star)))
                };
                if let Some((mapping, star)) = hit {
                    if let Some(p) = self.resolve_target(
                        mapping,
                        &current,
                        star.as_deref(),
                        true,
                        Some(importer),
                    )? {
                        return Ok(Some(p));
                    }
                    anyhow::bail!(
                        "package \"imports\" specifier '{specifier}' maps to {mapping} \
                         which does not exist"
                    );
                }
                anyhow::bail!(
                    "package \"imports\" specifier '{specifier}' is not defined in {}",
                    pkg_json_path.display()
                );
            }
            if !current.pop() {
                break;
            }
        }
        Ok(None)
    }
}

// Pattern-key matching for `exports`/`imports` maps lives in this crate's
// `package_map` module (shared with the build engine, which re-exports it as
// `pledgepack_core::package_map`).
use crate::package_map::best_pattern_match;

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
    fn scoped_package_subpath_honours_exports_map() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let pkg = root.join("node_modules/@scope/pkg");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/pkg","exports":{".":"./dist/index.js","./feat":"./dist/feature.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("dist/index.js"), "").unwrap();
        std::fs::write(pkg.join("dist/feature.js"), "").unwrap();
        let importer = root.join("src/main.js");

        let resolver = Resolver::new(root.clone(), vec![".js".to_string()], vec![]);
        let sub = resolver.resolve("@scope/pkg/feat", &importer).unwrap();
        assert!(sub.ends_with(Path::new("dist/feature.js")), "{sub:?}");
        let main = resolver.resolve("@scope/pkg", &importer).unwrap();
        assert!(main.ends_with(Path::new("dist/index.js")), "{main:?}");
    }

    #[test]
    fn scoped_package_found_in_pnpm_virtual_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let pkg = root.join("node_modules/.pnpm/@scope+pkg@1.0.0/node_modules/@scope/pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"@scope/pkg","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "").unwrap();

        let resolver = Resolver::new(root.clone(), vec![".js".to_string()], vec![]);
        let resolved = resolver
            .resolve("@scope/pkg", &root.join("src/main.js"))
            .unwrap();
        assert!(resolved.ends_with("index.js"), "{resolved:?}");
    }

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
        assert!(
            result.is_ok(),
            "pnpm virtual-store fallback failed: {:?}",
            result.err()
        );
        assert!(result.unwrap().ends_with("index.js"));
    }

    #[test]
    fn pnpm_nested_workspace_walks_up_to_root_node_modules() {
        // A monorepo package resolving a dependency hoisted to the
        // workspace root's node_modules, not its own — exercises the
        // "go up one directory" loop in `resolve_node_module`.
        let dir = tempfile::tempdir().unwrap();
        let root_pkg = dir.path().join("node_modules").join("hoisted-dep");
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
        assert!(
            result.is_ok(),
            "failed to walk up to root node_modules: {:?}",
            result.err()
        );
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
        assert!(
            result.is_ok(),
            "failed to resolve a long path: {:?}",
            result.err()
        );
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
        std::fs::write(
            dir.path().join("a.js"),
            "import './b.js'; export const a = 1;",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.js"),
            "import './a.js'; export const b = 2;",
        )
        .unwrap();

        let resolver = Resolver::new(dir.path().to_path_buf(), vec![".js".into()], vec![]);
        let a = dir.path().join("a.js");
        let b = dir.path().join("b.js");

        assert!(resolver.resolve("./b.js", &a).is_ok());
        assert!(resolver.resolve("./a.js", &b).is_ok());
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;

    fn w(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn resolver(root: &Path) -> Resolver {
        Resolver::new(
            root.to_path_buf(),
            vec![".ts".into(), ".tsx".into(), ".js".into()],
            vec![],
        )
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    #[test]
    fn exports_pattern_with_star_in_the_middle_of_the_key() {
        let (_d, root) = setup();
        let pkg = root.join("node_modules/pkg");
        w(
            &pkg.join("package.json"),
            r#"{"exports":{"./features/*.js":"./dist/features/*.js"}}"#,
        );
        w(&pkg.join("dist/features/a.js"), "");
        let got = resolver(&root)
            .resolve("pkg/features/a.js", &root.join("src/m.js"))
            .unwrap();
        assert!(got.ends_with(Path::new("dist/features/a.js")), "{got:?}");
    }

    #[test]
    fn exports_pattern_with_conditions_object() {
        let (_d, root) = setup();
        let pkg = root.join("node_modules/@s/pkg");
        w(
            &pkg.join("package.json"),
            r#"{"exports":{"./utils/*":{"import":"./esm/*.js","default":"./cjs/*.js"}}}"#,
        );
        w(&pkg.join("esm/x.js"), "");
        w(&pkg.join("cjs/x.js"), "");
        let got = resolver(&root)
            .resolve("@s/pkg/utils/x", &root.join("src/m.js"))
            .unwrap();
        assert!(got.ends_with(Path::new("esm/x.js")), "{got:?}");
    }

    #[test]
    fn exports_nested_conditions_and_arrays() {
        let (_d, root) = setup();
        let pkg = root.join("node_modules/pkg");
        w(
            &pkg.join("package.json"),
            r#"{"exports":{".":{"import":{"types":"./index.d.ts","default":"./esm/index.js"},"require":"./cjs/index.js"},"./arr":["./missing.js","./arr.js"]}}"#,
        );
        w(&pkg.join("esm/index.js"), "");
        w(&pkg.join("arr.js"), "");
        let r = resolver(&root);
        let importer = root.join("src/m.js");
        assert!(
            r.resolve("pkg", &importer)
                .unwrap()
                .ends_with(Path::new("esm/index.js"))
        );
        assert!(r.resolve("pkg/arr", &importer).unwrap().ends_with("arr.js"));
    }

    #[test]
    fn exports_target_cannot_escape_the_package() {
        let (_d, root) = setup();
        w(&root.join("secret.js"), "");
        let pkg = root.join("node_modules/pkg");
        w(
            &pkg.join("package.json"),
            r#"{"exports":{"./evil":"../../secret.js"}}"#,
        );
        assert!(
            resolver(&root)
                .resolve("pkg/evil", &root.join("src/m.js"))
                .is_err()
        );
    }

    #[test]
    fn bare_specifier_with_dotdot_subpath_cannot_escape_the_package() {
        let (_d, root) = setup();
        w(&root.join("secret.js"), "");
        w(
            &root.join("node_modules/pkg/package.json"),
            r#"{"main":"i.js"}"#,
        );
        w(&root.join("node_modules/pkg/i.js"), "");
        assert!(
            resolver(&root)
                .resolve("pkg/../../secret.js", &root.join("src/m.js"))
                .is_err()
        );
    }

    #[test]
    fn imports_field_patterns_and_bare_targets() {
        let (_d, root) = setup();
        w(
            &root.join("package.json"),
            r##"{"imports":{"#utils/*":"./src/utils/*.js","#dep":"dep-pkg","#cond":{"node":"./n.js","default":"./d.js"}}}"##,
        );
        w(&root.join("src/utils/fmt.js"), "");
        w(&root.join("d.js"), "");
        w(
            &root.join("node_modules/dep-pkg/package.json"),
            r#"{"main":"m.js"}"#,
        );
        w(&root.join("node_modules/dep-pkg/m.js"), "");
        let r = resolver(&root);
        let importer = root.join("src/app.js");
        assert!(
            r.resolve("#utils/fmt", &importer)
                .unwrap()
                .ends_with(Path::new("src/utils/fmt.js"))
        );
        assert!(r.resolve("#dep", &importer).unwrap().ends_with("m.js"));
        assert!(r.resolve("#cond", &importer).unwrap().ends_with("d.js"));
    }

    #[test]
    fn dotted_names_append_extension_instead_of_replacing() {
        let (_d, root) = setup();
        w(&root.join("src/foo.ts"), "");
        w(&root.join("src/foo.config.ts"), "");
        w(&root.join("src/util.ts"), "");
        let r = resolver(&root);
        let importer = root.join("src/main.ts");
        // `./foo.config` must NOT collapse to foo.ts
        assert!(
            r.resolve("./foo.config", &importer)
                .unwrap()
                .ends_with("foo.config.ts")
        );
        // TS-ESM convention: `./util.js` -> util.ts still works
        assert!(
            r.resolve("./util.js", &importer)
                .unwrap()
                .ends_with("util.ts")
        );
    }

    #[test]
    fn dependency_of_a_package_in_the_pnpm_store_resolves_from_the_importer() {
        // .pnpm/a@1/node_modules/{a, b}: `a` imports its sibling dep `b`,
        // which is NOT under <root>/node_modules at all.
        let (_d, root) = setup();
        let store = root.join("node_modules/.pnpm/a@1.0.0/node_modules");
        w(&store.join("a/package.json"), r#"{"main":"index.js"}"#);
        w(&store.join("a/index.js"), "");
        w(&store.join("b/package.json"), r#"{"main":"index.js"}"#);
        w(&store.join("b/index.js"), "");
        let got = resolver(&root)
            .resolve("b", &store.join("a/index.js"))
            .unwrap();
        assert!(got.starts_with(&store), "{got:?}");
        assert!(got.ends_with(Path::new("b/index.js")), "{got:?}");
    }

    #[test]
    fn package_local_node_modules_beats_root_node_modules() {
        let (_d, root) = setup();
        w(
            &root.join("node_modules/dep/package.json"),
            r#"{"main":"i.js"}"#,
        );
        w(&root.join("node_modules/dep/i.js"), "root");
        w(
            &root.join("packages/app/node_modules/dep/package.json"),
            r#"{"main":"i.js"}"#,
        );
        w(&root.join("packages/app/node_modules/dep/i.js"), "local");
        let got = resolver(&root)
            .resolve("dep", &root.join("packages/app/src/x.js"))
            .unwrap();
        assert!(got.starts_with(root.join("packages/app")), "{got:?}");
    }

    #[test]
    fn symlinked_package_resolves_to_its_real_path() {
        let (_d, root) = setup();
        let real = root.join("node_modules/.pnpm/pkg@1.0.0/node_modules/pkg");
        w(&real.join("package.json"), r#"{"main":"index.js"}"#);
        w(&real.join("index.js"), "");
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&real, root.join("node_modules/pkg")).is_ok();
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_dir(&real, root.join("node_modules/pkg")).is_ok();
        if !linked {
            eprintln!("skipping: cannot create symlinks on this runner");
            return;
        }
        let got = resolver(&root)
            .resolve("pkg", &root.join("src/m.js"))
            .unwrap();
        assert_eq!(got, real.join("index.js").canonicalize().unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_and_backslash_specifiers() {
        let (_d, root) = setup();
        w(&root.join("src/a.ts"), "");
        w(&root.join("src/b.ts"), "");
        let r = resolver(&root);
        let importer = root.join("src/b.ts");
        // drive-letter absolute path with backslashes
        let abs = root.join("src/a.ts");
        let abs_str = abs
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_string();
        assert!(r.resolve(&abs_str, &importer).is_ok(), "{abs_str}");
        // drive-letter absolute path with forward slashes
        assert!(r.resolve(&abs_str.replace('\\', "/"), &importer).is_ok());
        // `.\a` and `..\src\a` relative forms
        assert!(r.resolve(r".\a", &importer).is_ok());
        assert!(r.resolve(r"..\src\a", &importer).is_ok());
    }
}
