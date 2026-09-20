// End-to-end build benchmark: cold build, warm (task-cache) build, and
// incremental rebuild on a generated N-module app, plus a head-to-head of
// the legacy rayon transform path vs the task-graph transform path on the
// identical module set.
//
// Run with output visible:
//   cargo test -p pledgepack-core --test build_bench -- --nocapture
//
// Numbers are printed, not asserted — this is a measurement harness, not a
// regression gate. Peak-RSS is measured at the CI-job level (`/usr/bin/time
// -v` on Linux), matching the stress-test convention.

use pledgepack_core::config::{BuildMode, Framework, PledgeConfig};
use pledgepack_core::engine::BuildEngine;
use pledgepack_core::module::{ModuleId, ModuleKind, ResolvedModule};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

const NUM_MODULES: usize = 1_000;

/// Generated dep graph: every module i>=1 imports mod{i-1} plus a couple of
/// earlier modules, so importing the last module transitively reaches all.
fn module_source(index: usize) -> String {
    let mut src = String::new();
    if index > 0 {
        let num_imports = 1 + (index % 3);
        for i in 0..num_imports {
            let dep = index.saturating_sub(i + 1);
            src.push_str(&format!("import {{ value{dep} }} from \"./mod{dep}\";\n"));
        }
    }
    src.push_str(&format!(
        "export function value{index}(): number {{\n  return {index}"
    ));
    if index > 0 {
        let num_imports = 1 + (index % 3);
        for i in 0..num_imports {
            let dep = index.saturating_sub(i + 1);
            src.push_str(&format!(" + value{dep}()"));
        }
    }
    src.push_str(";\n}\n");
    src
}

/// Write a generated app to disk: `src/index.ts` entry + NUM_MODULES modules.
fn generate_app(root: &std::path::Path) -> PathBuf {
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let entry = format!(
        "import {{ value{} }} from \"./mod{}\";\nexport default value{};\n",
        NUM_MODULES - 1,
        NUM_MODULES - 1,
        NUM_MODULES - 1
    );
    std::fs::write(src.join("index.ts"), entry).unwrap();
    for i in 0..NUM_MODULES {
        std::fs::write(src.join(format!("mod{i}.ts")), module_source(i)).unwrap();
    }
    src
}

fn make_config(root: &std::path::Path) -> PledgeConfig {
    PledgeConfig {
        root: root.to_path_buf(),
        entry: vec!["src/index.ts".to_string()],
        mode: BuildMode::Production,
        framework: Framework::Pledge,
        cache: pledgepack_core::config::CacheConfig {
            dir: root.join(".cache"),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[tokio::test]
async fn bench_cold_warm_incremental_builds() {
    let tmp = TempDir::new().unwrap();
    generate_app(tmp.path());

    // ── Cold build: task pipeline, empty cache ──
    let t = Instant::now();
    let mut engine = BuildEngine::new(Arc::new(make_config(tmp.path())));
    let cold = engine.build().await.expect("cold build failed");
    let cold_ms = t.elapsed().as_millis();
    eprintln!(
        "[bench] cold build:    {:>5} ms  ({} built, {} cached)",
        cold_ms, cold.modules_built, cold.modules_cached
    );

    // ── Warm build: fresh engine, populated task disk cache ──
    let t = Instant::now();
    let mut engine = BuildEngine::new(Arc::new(make_config(tmp.path())));
    let warm = engine.build().await.expect("warm build failed");
    let warm_ms = t.elapsed().as_millis();
    eprintln!(
        "[bench] warm build:    {:>5} ms  ({} built, {} cached)",
        warm_ms, warm.modules_built, warm.modules_cached
    );

    // ── Incremental: one file changed, fresh engine ──
    std::fs::write(
        tmp.path().join("src/mod500.ts"),
        module_source(500).replace("return 500", "return 501"),
    )
    .unwrap();
    let t = Instant::now();
    let mut engine = BuildEngine::new(Arc::new(make_config(tmp.path())));
    let incr = engine.build().await.expect("incremental build failed");
    let incr_ms = t.elapsed().as_millis();
    eprintln!(
        "[bench] incremental:   {:>5} ms  ({} built, {} cached)",
        incr_ms, incr.modules_built, incr.modules_cached
    );

    // The task path's contract: a warm build re-transforms nothing, and a
    // one-file change re-transforms exactly that file (index.ts imports it
    // transitively but its own source is unchanged → same TaskId).
    assert_eq!(warm.modules_built, 0, "warm build must not re-transform");
    assert_eq!(
        warm.modules_cached,
        NUM_MODULES + 1,
        "warm build must serve every module from the task cache"
    );
    assert_eq!(
        incr.modules_built, 1,
        "incremental must rebuild only mod500"
    );
}

#[test]
fn bench_legacy_vs_task_transform_path() {
    // Same module list through both transform implementations — isolates
    // the memoization boundary (task engine) from everything else.
    let tmp = TempDir::new().unwrap();
    let modules: Vec<(ModuleId, ResolvedModule)> = (0..NUM_MODULES)
        .map(|i| {
            let source = module_source(i);
            (
                i as ModuleId,
                ResolvedModule {
                    id: i as ModuleId,
                    path: PathBuf::from(format!("src/mod{i}.ts")),
                    kind: ModuleKind::TypeScript,
                    content_hash: 0,
                    source: source.into_bytes(),
                },
            )
        })
        .collect();

    let mut engine = BuildEngine::new(Arc::new(make_config(tmp.path())));

    let t = Instant::now();
    let legacy = engine
        .transform_modules_parallel(modules.clone())
        .expect("legacy transform failed");
    let legacy_ms = t.elapsed().as_millis();

    // First task run is a cold cache; the second measures memoized reads.
    let t = Instant::now();
    let (task_out, hits) = engine
        .transform_modules_via_tasks(modules.clone())
        .expect("task transform failed");
    let task_cold_ms = t.elapsed().as_millis();

    let t = Instant::now();
    let (_task_out2, hits2) = engine
        .transform_modules_via_tasks(modules)
        .expect("task transform (warm) failed");
    let task_warm_ms = t.elapsed().as_millis();

    eprintln!(
        "[bench] transform {} modules:  legacy {:>5} ms | task cold {:>5} ms | task warm {:>5} ms ({} + {} hits)",
        NUM_MODULES, legacy_ms, task_cold_ms, task_warm_ms, hits, hits2
    );

    assert_eq!(legacy.len(), task_out.len());
    assert_eq!(hits2, NUM_MODULES, "second task run must be all hits");
}
