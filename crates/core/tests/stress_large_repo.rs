// PRODUCTION-READINESS-100.md goal 99: large-repo build stress test.
//
// Generates a synthetic 5,000-module TypeScript "repo" in memory and drives
// it through the real transform pipeline (`BuildEngine::transform_modules_
// parallel`), tracking wall-clock time. Scope note: this deliberately drives
// the pipeline through `BuildEngine`'s library API rather than the compiled
// `pledge` CLI binary — see PRODUCTION-READINESS-100.md's "Known blockers"
// section, the CLI binary currently segfaults on every invocation on at
// least one dev machine, so a true CLI-level end-to-end stress test isn't
// runnable right now. This test still exercises the real, shared transform
// code path the CLI would call into, just not through the binary.
//
// Peak-memory tracking is done at the CI-job level (wrapping this test with
// `/usr/bin/time -v` on Linux to read "Maximum resident set size"), not
// in-process — no memory-profiling crate is otherwise a dependency of this
// workspace, and adding one just to self-report an approximate number from
// inside the same process being measured would be less trustworthy than an
// OS-level measurement. See `.github/workflows/ci.yml`'s `stress-test` job.

use pledgepack_core::config::PledgeConfig;
use pledgepack_core::engine::BuildEngine;
use pledgepack_core::module::{ModuleId, ModuleKind, ResolvedModule};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const NUM_MODULES: usize = 5_000;

/// Build one synthetic module's source: a small TS module that imports a
/// handful of "earlier" modules (by index, wrapping so early modules stay
/// leaves) and exports a function — representative of a real app's shape
/// without needing 5,000 real files on disk.
fn synthetic_module_source(index: usize) -> String {
    let mut src = String::new();
    let num_imports = if index == 0 {
        0
    } else {
        (index % 5).min(index)
    };
    for i in 0..num_imports {
        let dep = index.saturating_sub(i + 1);
        src.push_str(&format!("import {{ value{dep} }} from \"./mod{dep}\";\n"));
    }
    src.push_str(&format!(
        "export function value{index}(): number {{\n  return {index}"
    ));
    for i in 0..num_imports {
        let dep = index.saturating_sub(i + 1);
        src.push_str(&format!(" + value{dep}()"));
    }
    src.push_str(";\n}\n");
    src
}

#[test]
fn transforms_5000_module_synthetic_repo_within_time_budget() {
    let config = Arc::new(PledgeConfig::default());
    let mut engine = BuildEngine::new(config);

    let modules: Vec<(ModuleId, ResolvedModule)> = (0..NUM_MODULES)
        .map(|i| {
            let source = synthetic_module_source(i);
            let id = i as ModuleId;
            (
                id,
                ResolvedModule {
                    id,
                    path: PathBuf::from(format!("mod{i}.ts")),
                    kind: ModuleKind::TypeScript,
                    content_hash: 0,
                    source: source.into_bytes(),
                },
            )
        })
        .collect();

    let start = Instant::now();
    let results = engine
        .transform_modules_parallel(modules)
        .expect("transforming the synthetic repo should not fail");
    let elapsed = start.elapsed();

    assert_eq!(
        results.len(),
        NUM_MODULES,
        "expected every synthetic module to transform successfully"
    );

    println!(
        "Transformed {NUM_MODULES} synthetic modules in {:?} ({:.2} modules/sec)",
        elapsed,
        NUM_MODULES as f64 / elapsed.as_secs_f64().max(0.001)
    );

    // Generous sanity ceiling, not a tight performance target — same
    // reasoning as goal 61's binary-size budget and goal 90's coverage
    // floor: there's no historical baseline to calibrate a tight budget
    // against yet, so this catches a catastrophic regression (e.g. an
    // accidental O(n^2) path) without being a source of flaky failures on
    // a loaded CI runner.
    assert!(
        elapsed.as_secs() < 120,
        "transforming {NUM_MODULES} synthetic modules took {:?}, over the 120s sanity ceiling",
        elapsed
    );
}
