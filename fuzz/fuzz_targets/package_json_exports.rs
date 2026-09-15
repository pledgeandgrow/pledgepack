//! Fuzzes the resolver's package.json `exports` field handling. The
//! `exports` map is arbitrary JSON supplied by whatever package the user (or
//! a transitive dependency) installed — untrusted input the resolver has to
//! parse without panicking. `resolve_exports` itself is private, so this
//! drives it indirectly through the public `Resolver::resolve()` API by
//! writing fuzzed content into a real package.json on disk, which is closer
//! to the real code path anyway (also exercises the surrounding
//! `serde_json` parsing and file I/O, not just the exports-walking logic in
//! isolation). See PRODUCTION-READINESS-100.md goal 29.
//!
//! Run with `cargo +nightly fuzz run package_json_exports` from `fuzz/`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pledgepack_resolver::Resolver;

fuzz_target!(|package_json_body: &str| {
    // Reject inputs that would produce a wildly oversized file — fuzzers
    // tend to find "make the input huge" long before anything interesting,
    // which just wastes cycles on I/O rather than parser logic.
    if package_json_body.len() > 64 * 1024 {
        return;
    }

    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let root = dir.path().to_path_buf();
    let pkg_dir = root.join("node_modules").join("fuzzed-pkg");
    if std::fs::create_dir_all(&pkg_dir).is_err() {
        return;
    }

    // The fuzzer controls the whole file body, not just the `exports` value
    // — it's just as valid (and just as much the resolver's problem to
    // handle without panicking) for the input to not even be a JSON object.
    if std::fs::write(pkg_dir.join("package.json"), package_json_body).is_err() {
        return;
    }
    std::fs::write(root.join("index.js"), "export default 1;").ok();

    let resolver = Resolver::new(root.clone(), vec![".js".into()], vec![]);
    let importer = root.join("index.js");

    // Exercise a few subpaths, not just the bare specifier, since
    // conditional/subpath exports are where the real parsing branches live.
    for specifier in ["fuzzed-pkg", "fuzzed-pkg/sub", "fuzzed-pkg/../escape"] {
        let _ = resolver.resolve(specifier, &importer);
    }
});
