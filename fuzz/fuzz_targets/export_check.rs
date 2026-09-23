#![no_main]

//! `export_check` statically validates named/default imports against exports.
//! Fuzz contract: arbitrary source pairs never panic; diagnostics may or may
//! not be produced but analysis always terminates.
use libfuzzer_sys::fuzz_target;
use std::path::{Path, PathBuf};

fuzz_target!(|data: &[u8]| {
    // Split input into an importer module and an imported module so
    // `check_imports` exercises cross-module analysis, not just parsing.
    let (a, b) = match data.iter().position(|b| *b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let importer_src = String::from_utf8_lossy(a).into_owned();
    let imported_src = String::from_utf8_lossy(b).into_owned();

    let root = Path::new("/proj");
    let importer = PathBuf::from("/proj/src/main.ts");
    let imported = PathBuf::from("/proj/src/util.ts");

    // Direct analysis — the no-panic contract on a single module.
    let _ = pledgepack_core::export_check::analyze_module(&importer_src, &importer);
    let _ = pledgepack_core::export_check::analyze_module(&imported_src, &imported);

    // Full cross-module check with a trivial resolver that maps every
    // specifier onto the imported file.
    let modules = vec![
        (importer.clone(), importer_src),
        (imported.clone(), imported_src),
    ];
    let mut resolve = move |_: &str, _: &Path| -> Option<PathBuf> { Some(imported.clone()) };
    let _ = pledgepack_core::export_check::check_imports(&modules, root, &mut resolve);
});
