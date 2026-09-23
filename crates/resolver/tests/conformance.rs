//! Resolver conformance fixture suite.
//!
//! Each test builds a small on-disk project fixture (tempdir) and asserts how
//! the real [`Resolver`] resolves specifiers through it — the same code path
//! `pledgepack-core`'s engine now delegates to. The matrix covers the modern
//! resolution surface bundlers disagree on:
//!
//! * `package.json` `exports` — condition priority (runtime + module type),
//!   subpath patterns, unlisted-subpath rejection;
//! * `package.json` `imports` — `#subpath` exact/pattern/conditional targets
//!   and Node's package-scope boundary;
//! * workspace packages (`Resolver::with_workspace`);
//! * `tsconfig.json` `paths`/`baseUrl`/`extends` (`Resolver::from_tsconfig`);
//! * the `browser` field — object map and string forms, gated on
//!   [`ResolveRuntime::Browser`].

use pledgepack_resolver::{
    Alias, ResolveContext, ResolveModuleType, ResolveRuntime, Resolver, WorkspacePackage,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".json"];

fn w(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    // Canonicalize so assertions line up with the resolver's canonical output
    // (Windows tempdirs otherwise differ in 8.3/case/verbatim form).
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

fn exts() -> Vec<String> {
    EXTENSIONS.iter().map(|s| s.to_string()).collect()
}

fn resolver(root: &Path) -> Resolver {
    Resolver::new(root.to_path_buf(), exts(), vec![])
}

fn resolver_with_context(root: &Path, ctx: ResolveContext) -> Resolver {
    Resolver::with_context(root.to_path_buf(), exts(), vec![], ctx)
}

fn ends_with(path: &Path, suffix: &str) -> bool {
    path.ends_with(Path::new(suffix))
}

// ── exports conditions ───────────────────────────────────────────────

#[test]
fn exports_conditions_follow_runtime_and_module_type() {
    let (_d, root) = setup();
    let pkg = root.join("node_modules/pkg");
    w(
        &pkg.join("package.json"),
        r#"{
            "main": "./cjs.js",
            "exports": {
                ".": {
                    "browser": { "import": "./browser.mjs", "require": "./browser.cjs" },
                    "node": "./node.mjs",
                    "default": "./default.mjs"
                }
            }
        }"#,
    );
    for f in [
        "browser.mjs",
        "browser.cjs",
        "node.mjs",
        "default.mjs",
        "cjs.js",
    ] {
        w(&pkg.join(f), "");
    }
    let importer = root.join("src/m.js");

    // Browser + ESM importer → browser.import
    let r = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Browser,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(
        ends_with(&r.resolve("pkg", &importer).unwrap(), "browser.mjs"),
        "{:?}",
        r.resolve("pkg", &importer)
    );

    // Browser + CJS importer → browser.require
    let r = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Browser,
            module_type: ResolveModuleType::Cjs,
        },
    );
    assert!(
        ends_with(&r.resolve("pkg", &importer).unwrap(), "browser.cjs"),
        "{:?}",
        r.resolve("pkg", &importer)
    );

    // Node runtime → node condition
    let r = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Node,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(
        ends_with(&r.resolve("pkg", &importer).unwrap(), "node.mjs"),
        "{:?}",
        r.resolve("pkg", &importer)
    );
}

#[test]
fn exports_pattern_subpaths_and_unlisted_subpath_rejection() {
    let (_d, root) = setup();
    let pkg = root.join("node_modules/pkg");
    w(
        &pkg.join("package.json"),
        r#"{
            "exports": {
                ".": "./index.js",
                "./features/*.js": "./dist/features/*.js",
                "./internal": null
            }
        }"#,
    );
    w(&pkg.join("index.js"), "");
    w(&pkg.join("dist/features/deep/x.js"), "");
    w(&pkg.join("dist/features/a.js"), "");
    w(&pkg.join("private.js"), "");
    let r = resolver(&root);
    let importer = root.join("src/m.js");

    assert!(ends_with(&r.resolve("pkg", &importer).unwrap(), "index.js"));
    assert!(ends_with(
        &r.resolve("pkg/features/a.js", &importer).unwrap(),
        "dist/features/a.js"
    ));
    // A file that exists on disk but is not exported must not resolve.
    assert!(r.resolve("pkg/private.js", &importer).is_err());
}

#[test]
fn exports_dotdot_escape_is_rejected_even_when_file_exists() {
    let (_d, root) = setup();
    w(&root.join("secret.js"), "");
    w(
        &root.join("node_modules/pkg/package.json"),
        r#"{"exports":{".":"../../secret.js"}}"#,
    );
    assert!(
        resolver(&root)
            .resolve("pkg", &root.join("src/m.js"))
            .is_err()
    );
}

// ── imports / #subpath ───────────────────────────────────────────────

#[test]
fn imports_field_exact_pattern_and_conditions() {
    let (_d, root) = setup();
    w(
        &root.join("package.json"),
        r##"{
            "imports": {
                "#config": "./config.js",
                "#utils/*": "./src/utils/*.js",
                "#platform": { "node": "./plat/node.js", "default": "./plat/web.js" }
            }
        }"##,
    );
    w(&root.join("config.js"), "");
    w(&root.join("src/utils/fmt.js"), "");
    w(&root.join("plat/node.js"), "");
    w(&root.join("plat/web.js"), "");
    let importer = root.join("src/app.js");

    let r = resolver(&root);
    assert!(ends_with(
        &r.resolve("#config", &importer).unwrap(),
        "config.js"
    ));
    assert!(ends_with(
        &r.resolve("#utils/fmt", &importer).unwrap(),
        "src/utils/fmt.js"
    ));

    let node = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Node,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(ends_with(
        &node.resolve("#platform", &importer).unwrap(),
        "plat/node.js"
    ));
    // Default runtime is Browser → "default" condition.
    assert!(ends_with(
        &r.resolve("#platform", &importer).unwrap(),
        "plat/web.js"
    ));
}

#[test]
fn imports_scope_stops_at_nearest_package_json_without_imports() {
    // Node semantics: the nearest package.json bounds the `imports` scope.
    // An inner package.json with no "imports" must NOT fall through to the
    // outer one that has it.
    let (_d, root) = setup();
    w(
        &root.join("package.json"),
        r##"{"imports":{"#dep":"./dep.js"}}"##,
    );
    w(&root.join("dep.js"), "");
    // Nested package without an imports field.
    w(&root.join("sub/package.json"), r#"{"name":"sub"}"#);
    let inner = root.join("sub/deep/m.js");
    w(&inner, "");

    assert!(
        resolver(&root).resolve("#dep", &inner).is_err(),
        "#dep must not resolve outside the inner package scope"
    );
    // …but resolves fine from a file directly under the outer package.
    assert!(
        resolver(&root)
            .resolve("#dep", &root.join("src/m.js"))
            .is_ok()
    );
}

#[test]
fn imports_bare_target_resolves_to_package() {
    let (_d, root) = setup();
    w(
        &root.join("package.json"),
        r##"{"imports":{"#shim":"dep-pkg"}}"##,
    );
    w(
        &root.join("node_modules/dep-pkg/package.json"),
        r#"{"main":"entry.js"}"#,
    );
    w(&root.join("node_modules/dep-pkg/entry.js"), "");
    assert!(ends_with(
        &resolver(&root)
            .resolve("#shim", &root.join("src/m.js"))
            .unwrap(),
        "entry.js"
    ));
}

// ── workspace links ──────────────────────────────────────────────────

#[test]
fn workspace_package_resolves_root_and_subpaths() {
    let (_d, root) = setup();
    let ui = root.join("packages/ui");
    w(&ui.join("package.json"), r#"{"name":"@acme/ui"}"#);
    w(&ui.join("dist/index.mjs"), "");
    w(&ui.join("dist/button.js"), "");
    w(&ui.join("src/util.ts"), "");

    let mut packages = HashMap::new();
    packages.insert(
        "@acme/ui".to_string(),
        WorkspacePackage {
            path: ui.clone(),
            main: None,
            module: Some("dist/index.mjs".into()),
            exports: Some(serde_json::json!({ "./button": "./dist/button.js" })),
        },
    );
    let r = Resolver::with_workspace(root.clone(), exts(), vec![], packages);
    let importer = root.join("src/app.ts");

    // Root → module field.
    assert!(ends_with(
        &r.resolve("@acme/ui", &importer).unwrap(),
        "dist/index.mjs"
    ));
    // Subpath → exports map.
    assert!(ends_with(
        &r.resolve("@acme/ui/button", &importer).unwrap(),
        "dist/button.js"
    ));
    // `exports` encapsulates the package: `src/util` exists on disk but is
    // not exported, so it must NOT resolve (Node PACKAGE_PATH_NOT_EXPORTED).
    assert!(r.resolve("@acme/ui/src/util", &importer).is_err());
}

#[test]
fn workspace_package_without_exports_probes_subpath_files() {
    let (_d, root) = setup();
    let ui = root.join("packages/ui");
    w(&ui.join("package.json"), r#"{"name":"@acme/ui"}"#);
    w(&ui.join("index.js"), "");
    w(&ui.join("src/util.ts"), "");

    let mut packages = HashMap::new();
    packages.insert(
        "@acme/ui".to_string(),
        WorkspacePackage {
            path: ui,
            main: Some("index.js".into()),
            ..WorkspacePackage::default()
        },
    );
    let r = Resolver::with_workspace(root.clone(), exts(), vec![], packages);
    let importer = root.join("src/app.ts");
    // No `exports` field → direct file + extension probing applies.
    assert!(ends_with(
        &r.resolve("@acme/ui/src/util", &importer).unwrap(),
        "src/util.ts"
    ));
}

#[test]
fn node_modules_package_beats_workspace_package() {
    // Resolution order puts node_modules after the workspace check in
    // `resolve_uncached` — assert whichever ordering is implemented stays
    // deterministic: the workspace map is consulted for bare specifiers and
    // wins when it produces a file.
    let (_d, root) = setup();
    let ws_pkg = root.join("packages/shared");
    w(&ws_pkg.join("index.js"), "");
    let mut packages = HashMap::new();
    packages.insert(
        "shared".to_string(),
        WorkspacePackage {
            path: ws_pkg,
            main: Some("index.js".into()),
            ..WorkspacePackage::default()
        },
    );
    let r = Resolver::with_workspace(root.clone(), exts(), vec![], packages);
    let got = r.resolve("shared", &root.join("src/m.js")).unwrap();
    assert!(got.starts_with(root.join("packages")), "{got:?}");
}

// ── tsconfig paths ───────────────────────────────────────────────────

#[test]
fn tsconfig_paths_baseurl_and_wildcards() {
    let (_d, root) = setup();
    w(
        &root.join("tsconfig.json"),
        r#"{
            "compilerOptions": {
                "baseUrl": ".",
                "paths": {
                    "@/*": ["src/*"],
                    "~utils": ["src/utils/index.ts"],
                    "lib/*": ["vendor/lib/*"]
                }
            }
        }"#,
    );
    w(&root.join("src/components/App.tsx"), "");
    w(&root.join("src/utils/index.ts"), "");
    w(&root.join("vendor/lib/math.ts"), "");
    let r = Resolver::from_tsconfig(root.clone(), exts());
    let importer = root.join("src/main.ts");

    assert!(ends_with(
        &r.resolve("@/components/App", &importer).unwrap(),
        "src/components/App.tsx"
    ));
    assert!(ends_with(
        &r.resolve("~utils", &importer).unwrap(),
        "src/utils/index.ts"
    ));
    assert!(ends_with(
        &r.resolve("lib/math", &importer).unwrap(),
        "vendor/lib/math.ts"
    ));
}

#[test]
fn tsconfig_extends_chain_and_jsconfig_fallback() {
    let (_d, root) = setup();
    // base tsconfig in a parent config dir, extended by the root one.
    w(
        &root.join("tsconfig.base.json"),
        r#"{"compilerOptions":{"paths":{"@core/*":["core/*"]}}}"#,
    );
    w(
        &root.join("tsconfig.json"),
        r#"{"extends":"./tsconfig.base.json","compilerOptions":{"paths":{"@app/*":["app/*"]}}}"#,
    );
    w(&root.join("core/kernel.ts"), "");
    w(&root.join("app/main.ts"), "");
    let r = Resolver::from_tsconfig(root.clone(), exts());
    let importer = root.join("index.ts");

    assert!(ends_with(
        &r.resolve("@app/main", &importer).unwrap(),
        "app/main.ts"
    ));
    assert!(ends_with(
        &r.resolve("@core/kernel", &importer).unwrap(),
        "core/kernel.ts"
    ));

    // jsconfig is used when no tsconfig exists.
    let (_d2, root2) = setup();
    w(
        &root2.join("jsconfig.json"),
        r#"{"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
    );
    w(&root2.join("src/x.js"), "");
    let r2 = Resolver::from_tsconfig(root2.clone(), exts());
    assert!(ends_with(
        &r2.resolve("@/x", &root2.join("m.js")).unwrap(),
        "src/x.js"
    ));
}

#[test]
fn explicit_alias_sorted_longest_first() {
    let (_d, root) = setup();
    w(&root.join("src/a.ts"), "");
    w(&root.join("src/atoms/b.ts"), "");
    let r = Resolver::new(
        root.clone(),
        exts(),
        vec![
            Alias {
                from: "@/".into(),
                to: root.join("src").to_string_lossy().to_string(),
            },
            Alias {
                from: "@/atoms/".into(),
                to: root.join("src/atoms").to_string_lossy().to_string(),
            },
        ],
    );
    let importer = root.join("src/m.ts");
    assert!(ends_with(&r.resolve("@/a", &importer).unwrap(), "src/a.ts"));
    assert!(ends_with(
        &r.resolve("@/atoms/b", &importer).unwrap(),
        "src/atoms/b.ts"
    ));
    // Boundary: "@/x-extra" must not match the "@/atoms/" prefix's siblings.
    assert!(r.resolve("@/atoms-extra/c", &importer).is_err());
}

#[test]
fn bare_alias_without_trailing_slash_joins_subpath() {
    // `from: "@"` (no trailing slash) must join the specifier remainder
    // *relative* to the target dir — a naive `Path::join` would treat the
    // remainder's leading slash as absolute and drop the target entirely.
    let (_d, root) = setup();
    w(&root.join("src/components/button.tsx"), "");
    let r = Resolver::new(
        root.clone(),
        exts(),
        vec![Alias {
            from: "@".into(),
            to: root.join("src").to_string_lossy().to_string(),
        }],
    );
    let importer = root.join("src/m.ts");
    assert!(ends_with(
        &r.resolve("@/components/button", &importer).unwrap(),
        "src/components/button.tsx"
    ));
}

// ── browser field ────────────────────────────────────────────────────

#[test]
fn browser_field_object_map_overrides_entry_and_subpaths() {
    let (_d, root) = setup();
    let pkg = root.join("node_modules/pkg");
    w(
        &pkg.join("package.json"),
        r#"{
            "main": "lib/node.js",
            "browser": {
                "./lib/node.js": "./lib/browser.js",
                "./lib/fs.js": "./lib/fs.web.js"
            }
        }"#,
    );
    w(&pkg.join("lib/node.js"), "");
    w(&pkg.join("lib/browser.js"), "");
    w(&pkg.join("lib/fs.js"), "");
    w(&pkg.join("lib/fs.web.js"), "");
    let importer = root.join("src/m.js");

    // Browser runtime: entry and subpath both redirected.
    let browser = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Browser,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(ends_with(
        &browser.resolve("pkg", &importer).unwrap(),
        "lib/browser.js"
    ));
    assert!(ends_with(
        &browser.resolve("pkg/lib/fs.js", &importer).unwrap(),
        "lib/fs.web.js"
    ));

    // Node runtime: browser field ignored entirely.
    let node = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Node,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(ends_with(
        &node.resolve("pkg", &importer).unwrap(),
        "lib/node.js"
    ));
    assert!(ends_with(
        &node.resolve("pkg/lib/fs.js", &importer).unwrap(),
        "lib/fs.js"
    ));
}

#[test]
fn browser_field_string_replaces_entry_only_in_browser() {
    let (_d, root) = setup();
    let pkg = root.join("node_modules/pkg");
    w(
        &pkg.join("package.json"),
        r#"{"main":"node.js","browser":"shim.js"}"#,
    );
    w(&pkg.join("node.js"), "");
    w(&pkg.join("shim.js"), "");
    let importer = root.join("src/m.js");

    let browser = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Browser,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(ends_with(
        &browser.resolve("pkg", &importer).unwrap(),
        "shim.js"
    ));
    let node = resolver_with_context(
        &root,
        ResolveContext {
            runtime: ResolveRuntime::Node,
            module_type: ResolveModuleType::Esm,
        },
    );
    assert!(ends_with(
        &node.resolve("pkg", &importer).unwrap(),
        "node.js"
    ));
}
