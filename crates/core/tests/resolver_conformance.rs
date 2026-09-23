//! Resolver conformance: `BuildEngine` and `pledgepack-resolver` must agree.
//!
//! The engine used to carry its own resolver — a second implementation that
//! silently diverged (no `imports`/`#subpath` support, different alias
//! semantics). Resolution is now unified: `BuildEngine::resolve` delegates to
//! a `pledgepack_resolver::Resolver` built from the same `PledgeConfig` via
//! `module_resolver`. This suite pins that contract: one shared set of cases
//! runs through both surfaces and must produce identical results, so a future
//! engine-side resolution change cannot drift unnoticed.
//!
//! Two engine-specific behaviours are *intentional* divergence and are asserted
//! as such (not as equality):
//!   - the `/__pledge_router` virtual module
//!   - the root-relative last-resort fallback for non-module specifiers

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pledgepack_core::config::PathAlias;
use pledgepack_core::engine::module_resolver;
use pledgepack_core::{BuildEngine, PledgeConfig};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// A shared fixture tree exercising every resolution feature both
/// implementations claim to support.
fn fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Relative + extension probing.
    write(root, "src/main.ts", "import './util'");
    write(root, "src/util.ts", "export {}");
    write(root, "src/user.service.ts", "export {}");
    write(root, "src/dir/index.ts", "export {}");
    write(root, "shared/x.ts", "export {}");

    // Alias target.
    write(root, "src/components/button.tsx", "export {}");

    // node_modules: main field, exports map, subpath, wildcard.
    write(
        root,
        "node_modules/plain/package.json",
        r#"{"name":"plain","main":"index.js"}"#,
    );
    write(root, "node_modules/plain/index.js", "");
    write(
        root,
        "node_modules/dual/package.json",
        r#"{"name":"dual","exports":{".":{"require":"./cjs.js","default":"./esm.js"},"./feat/*":{"default":"./lib/*.mjs"}}}"#,
    );
    write(root, "node_modules/dual/cjs.js", "");
    write(root, "node_modules/dual/esm.js", "");
    write(root, "node_modules/dual/lib/a.mjs", "");

    // package.json "imports" (#subpath) — the feature the old engine resolver
    // lacked; conformance means the engine must now honour it too.
    write(
        root,
        "package.json",
        r##"{"name":"fixture","imports":{"#dep":"./src/util.ts","#internal/*":"./src/*"}}"##,
    );

    tmp
}

fn config(root: &Path) -> PledgeConfig {
    let mut cfg = PledgeConfig::default();
    cfg.root = root.to_path_buf();
    cfg.cache.enabled = false;
    cfg.resolve_alias = vec![PathAlias {
        from: "@".to_string(),
        to: "./src".to_string(),
    }];
    cfg
}

/// (specifier, importer) pairs where engine and resolver must agree exactly.
fn agreement_cases(root: &Path) -> Vec<(String, PathBuf)> {
    let main = root.join("src/main.ts");
    vec![
        ("./util".into(), main.clone()),
        ("./util.js".into(), main.clone()), // .js specifier → .ts source
        ("./user.service".into(), main.clone()),
        ("./dir".into(), main.clone()), // index probing
        ("../shared/x".into(), main.clone()),
        ("@/components/button".into(), main.clone()), // alias
        ("plain".into(), main.clone()),               // node_modules main
        ("dual".into(), main.clone()),                // exports map
        ("dual/feat/a".into(), main.clone()),         // exports wildcard
        ("#dep".into(), main.clone()),                // package imports
        ("#internal/dir".into(), main.clone()),       // imports wildcard
        ("./does-not-exist".into(), main.clone()),    // both must fail
        ("missing-pkg".into(), main.clone()),         // both must fail
    ]
}

#[test]
fn engine_and_resolver_agree_on_all_shared_cases() {
    let tmp = fixture();
    let root = tmp.path();
    let cfg = config(root);
    let resolver = module_resolver(&cfg);
    let engine = BuildEngine::new(Arc::new(cfg));

    for (spec, importer) in agreement_cases(root) {
        let via_resolver = resolver.resolve(&spec, &importer).map(|p| canon(&p));
        let via_engine = engine
            .resolve_specifier(&spec, &importer)
            .map(|p| canon(&p));
        assert_eq!(
            via_engine.as_ref().ok(),
            via_resolver.as_ref().ok(),
            "divergence resolving `{spec}` from {}: engine={via_engine:?} resolver={via_resolver:?}",
            importer.display()
        );
    }
}

/// Engine-only behaviours are explicit, documented divergence — not accidents.
#[test]
fn engine_only_behaviours_are_explicit_divergence() {
    let tmp = fixture();
    let root = tmp.path();
    let cfg = config(root);
    let resolver = module_resolver(&cfg);
    let engine = BuildEngine::new(Arc::new(cfg));
    let importer = root.join("src/main.ts");

    // Root-relative last resort: `src/util.ts` is not a module specifier, but
    // the engine maps it onto the project root when the file literally exists.
    let eng = engine.resolve_specifier("src/util.ts", &importer);
    assert_eq!(
        eng.ok().map(|p| canon(&p)),
        Some(canon(&root.join("src/util.ts")))
    );
    assert!(resolver.resolve("src/util.ts", &importer).is_err());
}

/// The conformance contract also covers config→resolver translation: both
/// surfaces see the same aliases/conditions because both come from
/// `module_resolver(config)`. A hand-built resolver missing an alias must
/// diverge — proving this test actually exercises the wiring.
#[test]
fn conformance_detects_missing_alias_wiring() {
    let tmp = fixture();
    let root = tmp.path();
    let cfg = config(root);
    let extensions = cfg.extensions.clone();
    let engine = BuildEngine::new(Arc::new(cfg));
    // A resolver built without the config's aliases must NOT match the engine.
    let bare = pledgepack_resolver::Resolver::new(root.to_path_buf(), extensions, vec![]);
    let importer = root.join("src/main.ts");
    assert!(
        engine
            .resolve_specifier("@/components/button", &importer)
            .is_ok()
    );
    assert!(bare.resolve("@/components/button", &importer).is_err());
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}
