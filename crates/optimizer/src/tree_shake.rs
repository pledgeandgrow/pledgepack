//! Export-aware module tree shaking.
//!
//! The graph's BFS reachability keeps every transitively imported module.
//! This pass goes one level deeper: it tracks *which bindings* each import
//! statement demands, propagates that demand through re-export chains
//! (`export { a } from "./a"` barrel files), and drops a reachable module
//! when every edge into it is provably dead —
//!
//!   * `import "x"` (side-effect-only) where `x` has no side effects, or
//!   * a binding/re-export edge whose demanded names are all unused, again
//!     only when the target has no side effects.
//!
//! A module "has no side effects" when its AST has no eval-time effects
//! ([`crate::side_effects`]) *or* its nearest `package.json` declares
//! `"sideEffects": false` (or an array that doesn't list the file) — the
//! webpack/rollup convention for dead-code-friendly packages.
//!
//! Dropping is at module granularity: the emit layer still writes a
//! `__pp.def` stub so importers' `__pp.req` calls resolve to an empty
//! exports object — this is why dead edges are only removed when the target
//! cannot have run side effects anyway.

use crate::side_effects::{self, ModuleAnalysis};
use pledgepack_core::module::{ModuleId, ModuleKind, ResolvedModule};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Module kinds eligible for dropping. Other kinds (CSS, JSON, assets,
/// Vue/Svelte/Astro components) feed non-JS pipelines where a "dead" module
/// still produces output (e.g. a CSS file), so they are always kept.
fn eligible_kind(kind: ModuleKind) -> bool {
    matches!(
        kind,
        ModuleKind::JavaScript | ModuleKind::TypeScript | ModuleKind::Jsx | ModuleKind::Tsx
    )
}

/// `package.json` `"sideEffects"` for the package containing `module_path`.
///
/// * `Some(true)` — the module is declared side-effect-free.
/// * `Some(false)` — declared (or defaulted) to have side effects.
/// * `None` — no `package.json` with a `sideEffects` field found; the caller
///   falls back to AST analysis.
///
/// Results are cached per package directory.
fn package_side_effect_free(
    module_path: &Path,
    cache: &mut HashMap<PathBuf, Option<bool>>,
) -> Option<bool> {
    let mut dir = module_path.parent();
    while let Some(d) = dir {
        let pkg_json = d.join("package.json");
        if pkg_json.is_file() {
            if let Some(v) = cache.get(d) {
                return *v;
            }
            let value = std::fs::read_to_string(&pkg_json)
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .and_then(|pkg| side_effect_flag(&pkg, module_path, d));
            cache.insert(d.to_path_buf(), value);
            return value;
        }
        dir = d.parent();
    }
    None
}

/// Interpret one package.json's `sideEffects` field for `module_path`.
fn side_effect_flag(pkg: &serde_json::Value, module_path: &Path, pkg_dir: &Path) -> Option<bool> {
    match pkg.get("sideEffects") {
        Some(serde_json::Value::Bool(b)) => Some(!b),
        Some(serde_json::Value::Array(list)) => {
            // Array lists the files that DO have side effects; anything not
            // matched is free. Patterns are package-relative globs.
            let rel = module_path
                .strip_prefix(pkg_dir)
                .unwrap_or(module_path)
                .to_string_lossy()
                .replace('\\', "/");
            let mut builder = globset::GlobSetBuilder::new();
            for pat in list.iter().filter_map(|v| v.as_str()) {
                let pat = pat.trim_start_matches("./");
                if let Ok(g) = globset::Glob::new(pat) {
                    builder.add(g);
                }
            }
            let set = builder.build().ok()?;
            Some(!set.is_match(&rel))
        }
        // Field absent on this package.json → keep looking? No — bundler
        // semantics consult the *nearest* package.json; an absent field
        // means "unknown", and the caller falls back to AST analysis.
        _ => Some(false),
    }
}

/// Normalized path for comparison: forward slashes, `.`/`..` resolved
/// lexically, lowercase (Windows/macOS filesystems are case-insensitive).
fn normalize(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy().to_string();
        match s.as_str() {
            "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(s),
        }
    }
    // Keep a root/prefix component ("/", "C:\") intact: components() already
    // yields Prefix/RootDir as their own items with a non-".." os_str, so the
    // loop above preserves them.
    parts.join("/").to_lowercase()
}

/// Candidate on-disk paths for a relative specifier — the bare path, each
/// extension appended, and `index.*` inside it as a directory.
fn relative_candidates(base_dir: &Path, spec: &str) -> Vec<String> {
    const EXTS: &[&str] = &[
        "ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "json", "css", "vue", "svelte",
    ];
    let joined = base_dir.join(spec);
    let mut out = vec![normalize(&joined)];
    for e in EXTS {
        out.push(normalize(&joined.with_extension(e)));
        out.push(normalize(&joined.join(format!("index.{e}"))));
    }
    // `./x.js` may point at `x.ts` (TS ESM convention).
    if let Some(ext) = joined.extension().and_then(|e| e.to_str())
        && matches!(ext, "js" | "jsx" | "mjs" | "cjs")
    {
        for e in ["ts", "tsx", "js", "jsx"] {
            out.push(normalize(&joined.with_extension(e)));
        }
    }
    out
}

/// Package name a bare specifier refers to (`pkg`, `@scope/pkg`).
fn bare_package_name(spec: &str) -> Option<&str> {
    if spec.starts_with('.') || spec.starts_with('/') || spec.starts_with('#') || spec.is_empty() {
        return None;
    }
    if spec.starts_with('@') {
        // "@scope/name[/sub…]" — the package ends at the second '/'.
        let mut slashes = spec.match_indices('/');
        slashes.next()?; // the scope separator must exist for a scoped name
        Some(match slashes.next() {
            Some((idx, _)) => &spec[..idx],
            None => spec,
        })
    } else {
        spec.split('/').next()
    }
}

/// Map one parsed specifier to the dep ModuleId it resolved to.
///
/// Relative specifiers are probed against dep paths directly; bare specifiers
/// match the unique dep inside `node_modules/<pkg>/`. Aliases and ambiguous
/// matches return `None` — treated as unconditionally-keeping edges.
fn match_dep(
    spec: &str,
    importer_dir: &Path,
    dep_ids: &[ModuleId],
    dep_paths: &HashMap<ModuleId, String>,
) -> Option<ModuleId> {
    if spec.starts_with("./") || spec.starts_with("../") || spec == "." || spec == ".." {
        let candidates = relative_candidates(importer_dir, spec);
        dep_ids
            .iter()
            .find(|id| {
                dep_paths
                    .get(id)
                    .is_some_and(|p| candidates.iter().any(|c| c == p))
            })
            .copied()
    } else if let Some(pkg) = bare_package_name(spec) {
        let needle = format!("/node_modules/{}/", pkg.to_lowercase());
        let mut hits = dep_ids
            .iter()
            .filter(|id| dep_paths.get(id).is_some_and(|p| p.contains(&needle)));
        match (hits.next(), hits.next()) {
            (Some(id), None) => Some(*id),
            _ => None,
        }
    } else {
        // Absolute and `#`-imports never reach bare-specifier dep edges.
        None
    }
}

/// The demand an edge places on its target.
struct Edge {
    target: ModuleId,
    /// Names bound by `import {…}` / `import d` — always demanded.
    import_names: Vec<String>,
    /// `(imported, exported)` re-export pairs — `imported` is demanded only
    /// while `exported` is in `used_exports(source)`.
    reexports: Vec<(String, String)>,
    all: bool,
}

/// Compute the set of modules that must keep their real implementation.
///
/// Everything in the returned set is emitted normally; `reachable` members
/// absent from it are emitted as empty `__pp.def` stubs by the emit layer.
pub fn needed_modules(
    entry_modules: &[ModuleId],
    reachable: &HashSet<ModuleId>,
    modules: &HashMap<ModuleId, ResolvedModule>,
    analyses: &HashMap<ModuleId, ModuleAnalysis>,
    graph: &pledgepack_core::Graph,
) -> HashSet<ModuleId> {
    // Normalized dep paths for specifier matching.
    let dep_paths: HashMap<ModuleId, String> = modules
        .iter()
        .map(|(id, m)| (*id, normalize(&m.path)))
        .collect();

    // Package-level sideEffects flag, cached per package dir.
    let mut pkg_flag_cache: HashMap<PathBuf, Option<bool>> = HashMap::new();
    let free: HashMap<ModuleId, bool> = modules
        .iter()
        .map(|(id, m)| {
            let eligible = eligible_kind(m.kind);
            let pkg_free = package_side_effect_free(&m.path, &mut pkg_flag_cache);
            let ast_free = analyses.get(id).is_some_and(|a| !a.has_side_effects);
            (*id, eligible && pkg_free.unwrap_or(ast_free))
        })
        .collect();

    // Classify each module's edges against its graph dependencies.
    let mut edges_of: HashMap<ModuleId, Vec<Edge>> = HashMap::new();
    let mut unmatched: HashMap<ModuleId, Vec<ModuleId>> = HashMap::new();
    for (&id, module) in modules {
        if !reachable.contains(&id) {
            continue;
        }
        let deps = graph.get_all_dependencies(id);
        let mut edges: Vec<Edge> = Vec::new();
        let mut covered: HashSet<ModuleId> = HashSet::new();
        if let Some(analysis) = analyses.get(&id) {
            let importer_dir = module.path.parent().unwrap_or(Path::new("."));
            for pe in &analysis.edges {
                if let Some(target) = match_dep(&pe.specifier, importer_dir, &deps, &dep_paths) {
                    covered.insert(target);
                    edges.push(Edge {
                        target,
                        import_names: pe.import_names.clone(),
                        reexports: pe.reexports.clone(),
                        all: pe.all,
                    });
                }
                // Unmatched specifiers (aliases, externals) simply don't
                // constrain any dep — the dep stays covered by `unmatched`.
            }
        }
        let rest: Vec<ModuleId> = deps.into_iter().filter(|d| !covered.contains(d)).collect();
        edges_of.insert(id, edges);
        unmatched.insert(id, rest);
    }

    // Demand fixpoint: `needed` modules and `used_exports` grow monotonically.
    let mut needed: HashSet<ModuleId> = entry_modules.iter().copied().collect();
    let mut used: HashMap<ModuleId, HashSet<String>> = HashMap::new();
    let mut all_used: HashSet<ModuleId> = HashSet::new();

    loop {
        let mut changed = false;

        // Propagate used export names across edges whose source is needed.
        for (&src, edges) in &edges_of {
            if !needed.contains(&src) {
                continue;
            }
            let src_all = all_used.contains(&src);
            for e in edges {
                let mut names: Vec<String> = e.import_names.clone();
                let u = used.get(&src);
                for (imported, exported) in &e.reexports {
                    if src_all || u.is_some_and(|u| u.contains(exported)) {
                        names.push(imported.clone());
                    }
                }
                if e.all && all_used.insert(e.target) {
                    changed = true;
                }
                let entry = used.entry(e.target).or_default();
                for n in names {
                    if entry.insert(n) {
                        changed = true;
                    }
                }
            }
        }

        // Keep deps whose edges demand something; unmatched edges keep too.
        for (&src, edges) in &edges_of {
            if !needed.contains(&src) {
                continue;
            }
            let src_all = all_used.contains(&src);
            for e in edges {
                let keeps = if e.all {
                    true
                } else {
                    let demanded = !e.import_names.is_empty()
                        || e.reexports.iter().any(|(_, exported)| {
                            src_all || used.get(&src).is_some_and(|u| u.contains(exported))
                        });
                    demanded || !free.get(&e.target).copied().unwrap_or(false)
                };
                if keeps && needed.insert(e.target) {
                    changed = true;
                }
            }
            for &d in unmatched.get(&src).into_iter().flatten() {
                if needed.insert(d) {
                    changed = true;
                }
            }
        }

        if !changed {
            break;
        }
    }

    needed
}

/// Run the module analysis for every JS-ish module (parallel), returning
/// `ModuleAnalysis` keyed by module id.
pub fn analyze_all(
    modules: &HashMap<ModuleId, ResolvedModule>,
) -> HashMap<ModuleId, ModuleAnalysis> {
    use rayon::prelude::*;
    modules
        .par_iter()
        .filter_map(|(id, m)| {
            if !eligible_kind(m.kind) {
                return None;
            }
            let source = String::from_utf8_lossy(&m.source);
            let source_type =
                oxc::span::SourceType::from_path(&m.path).unwrap_or(oxc::span::SourceType::mjs());
            Some((*id, side_effects::analyze_module(&source, source_type)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pledgepack_core::Graph;
    use pledgepack_core::module::{ModuleKind, ResolvedModule};

    fn module(id: ModuleId, path: &str, source: &str) -> ResolvedModule {
        ResolvedModule {
            id,
            path: PathBuf::from(path),
            kind: ModuleKind::JavaScript,
            source: source.as_bytes().to_vec(),
            content_hash: 0,
        }
    }

    /// Build graph + module table from `(path, source, deps)` rows; deps are
    /// given as indices into `rows`.
    fn fixture(rows: &[(&str, &str, &[usize])]) -> (Graph, HashMap<ModuleId, ResolvedModule>) {
        let graph = Graph::new();
        let mut modules = HashMap::new();
        for (path, source, _) in rows {
            let id = graph.add_module(path);
            modules.insert(id, module(id, path, source));
        }
        for (i, (_, _, deps)) in rows.iter().enumerate() {
            for &d in *deps {
                graph.add_dependency(i as ModuleId, d as ModuleId);
            }
        }
        (graph, modules)
    }

    fn analyses(modules: &HashMap<ModuleId, ResolvedModule>) -> HashMap<ModuleId, ModuleAnalysis> {
        analyze_all(modules)
    }

    fn reachable_all(modules: &HashMap<ModuleId, ResolvedModule>) -> HashSet<ModuleId> {
        modules.keys().copied().collect()
    }

    #[test]
    fn barrel_reexports_prune_unused_members() {
        // entry → barrel → {a.js, b.js}; only `a` is imported, so b.js drops.
        let (graph, modules) = fixture(&[
            (
                "/p/entry.js",
                "import { a } from './barrel';\nconsole.log(a);",
                &[1],
            ),
            (
                "/p/barrel.js",
                "export { a } from './a';\nexport { b } from './b';",
                &[2, 3],
            ),
            ("/p/a.js", "export const a = 1;", &[]),
            ("/p/b.js", "export const b = 2;", &[]),
        ]);
        let needed = needed_modules(
            &[0],
            &reachable_all(&modules),
            &modules,
            &analyses(&modules),
            &graph,
        );
        assert!(needed.contains(&0));
        assert!(needed.contains(&1));
        assert!(needed.contains(&2));
        assert!(!needed.contains(&3), "unused re-export must be dropped");
    }

    #[test]
    fn side_effect_only_import_of_pure_module_is_dropped() {
        let (graph, modules) = fixture(&[
            ("/p/entry.js", "import './dead';\nexport const e = 1;", &[1]),
            ("/p/dead.js", "export const x = 1;", &[]),
        ]);
        let needed = needed_modules(
            &[0],
            &reachable_all(&modules),
            &modules,
            &analyses(&modules),
            &graph,
        );
        assert!(!needed.contains(&1));
    }

    #[test]
    fn side_effect_only_import_of_impure_module_is_kept() {
        let (graph, modules) = fixture(&[
            (
                "/p/entry.js",
                "import './polyfill';\nexport const e = 1;",
                &[1],
            ),
            ("/p/polyfill.js", "globalThis.__x = 1;", &[]),
        ]);
        let needed = needed_modules(
            &[0],
            &reachable_all(&modules),
            &modules,
            &analyses(&modules),
            &graph,
        );
        assert!(needed.contains(&1), "side-effectful dep must survive");
    }

    #[test]
    fn named_import_keeps_module_even_when_pure() {
        // `import { x }` binds a name — conservative keep regardless of use.
        let (graph, modules) = fixture(&[
            (
                "/p/entry.js",
                "import { x } from './m';\nexport const e = 1;",
                &[1],
            ),
            ("/p/m.js", "export const x = 1;", &[]),
        ]);
        let needed = needed_modules(
            &[0],
            &reachable_all(&modules),
            &modules,
            &analyses(&modules),
            &graph,
        );
        assert!(needed.contains(&1));
    }

    #[test]
    fn star_reexport_keeps_everything() {
        let (graph, modules) = fixture(&[
            ("/p/entry.js", "import { a } from './barrel';", &[1]),
            (
                "/p/barrel.js",
                "export * from './a';\nexport * from './b';",
                &[2, 3],
            ),
            ("/p/a.js", "export const a = 1;", &[]),
            ("/p/b.js", "export const b = 2;", &[]),
        ]);
        let needed = needed_modules(
            &[0],
            &reachable_all(&modules),
            &modules,
            &analyses(&modules),
            &graph,
        );
        assert!(needed.contains(&3), "export * is opaque — keep");
    }

    #[test]
    fn multilevel_barrel_demand_flows_through() {
        // entry imports {a} from index; index re-exports {a} from mid; mid
        // re-exports {a} from a.js and {b} from b.js — b must drop.
        let (graph, modules) = fixture(&[
            ("/p/entry.js", "import { a } from './index';", &[1]),
            ("/p/index.js", "export { a } from './mid';", &[2]),
            (
                "/p/mid.js",
                "export { a } from './a';\nexport { b } from './b';",
                &[3, 4],
            ),
            ("/p/a.js", "export const a = 1;", &[]),
            ("/p/b.js", "export const b = 2;", &[]),
        ]);
        let needed = needed_modules(
            &[0],
            &reachable_all(&modules),
            &modules,
            &analyses(&modules),
            &graph,
        );
        assert!(needed.contains(&3));
        assert!(!needed.contains(&4));
    }
}
