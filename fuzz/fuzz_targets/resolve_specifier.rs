//! Fuzzes `Resolver::resolve()` against a small fixed fixture project,
//! mutating only the import specifier. Module resolution runs on every
//! import statement in every file a user bundles, including specifiers that
//! ultimately come from third-party/transitive dependencies the resolver's
//! caller doesn't control — see PRODUCTION-READINESS-100.md goal 29.
//!
//! Run with `cargo +nightly fuzz run resolve_specifier` from `fuzz/`
//! (requires `cargo install cargo-fuzz` on a nightly toolchain).
#![no_main]

use libfuzzer_sys::fuzz_target;
use pledgepack_resolver::{Alias, Resolver};
use std::path::PathBuf;
use std::sync::OnceLock;

/// A small fixture project, materialized once into a tempdir shared across
/// fuzz iterations (fuzzing only varies the specifier string, not the
/// on-disk layout, so there's no need to rebuild it per input).
struct Fixture {
    #[allow(dead_code)] // kept alive for the tempdir's Drop
    dir: tempfile::TempDir,
    root: PathBuf,
    importer: PathBuf,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let dir = tempfile::tempdir().expect("create fixture tempdir");
        let root = dir.path().to_path_buf();

        std::fs::write(root.join("index.js"), "export default 1;").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("util.ts"), "export const x = 1;").unwrap();

        // A scoped package with a conditional `exports` map, exercising the
        // resolver's exports-field / conditions logic, not just plain paths.
        let pkg_dir = root.join("node_modules").join("some-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{
                "name": "some-pkg",
                "exports": {
                    ".": { "import": "./esm/index.js", "require": "./cjs/index.js" },
                    "./sub": "./sub.js"
                }
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("esm")).unwrap();
        std::fs::write(pkg_dir.join("esm").join("index.js"), "export default 1;").unwrap();
        std::fs::write(pkg_dir.join("sub.js"), "export default 1;").unwrap();

        // A symlink-adjacent case (pnpm-style): a broken symlink, since
        // that's the case `crates/resolver` handles via `.unwrap_or(path)`
        // on `canonicalize()` failure — worth exercising directly.
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(
                root.join("does-not-exist.js"),
                root.join("broken-link.js"),
            );
        }

        let importer = root.join("index.js");
        Fixture {
            dir,
            root,
            importer,
        }
    })
}

fuzz_target!(|specifier: &str| {
    let f = fixture();
    let resolver = Resolver::new(
        f.root.clone(),
        vec![".ts".into(), ".tsx".into(), ".js".into(), ".jsx".into()],
        vec![Alias {
            from: "@/".to_string(),
            to: f.root.join("src").to_string_lossy().to_string(),
        }],
    );

    // The only property under test: resolution must never panic, hang, or
    // (per goal 51 in a later phase) escape the fixture root — it may
    // legitimately return Ok or Err for arbitrary fuzzed input.
    let _ = resolver.resolve(specifier, &f.importer);
});
