#![no_main]

//! `migrate_config` statically evaluates Vite/webpack/CRA/Next config files
//! into `pledge.config.ts`. Fuzz contract: arbitrary config files never panic
//! — unrecognized or malformed sources produce Err/unsupported, never a crash.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Rotate through the supported source-config names so every detection
    // branch is reachable from one target.
    const NAMES: &[&str] = &[
        "vite.config.ts",
        "vite.config.js",
        "webpack.config.js",
        "next.config.mjs",
        "config-overrides.js",
    ];
    let name = NAMES[data
        .first()
        .map(|b| (b % NAMES.len() as u8) as usize)
        .unwrap_or(0)];
    std::fs::write(root.join(name), data).unwrap();
    // A package.json makes migrations take the deeper merge paths.
    std::fs::write(
        root.join("package.json"),
        br#"{"name":"fuzz","dependencies":{"react":"latest"}}"#,
    )
    .unwrap();

    let _ = pledgepack_core::migrate::migrate_config(root);
});
