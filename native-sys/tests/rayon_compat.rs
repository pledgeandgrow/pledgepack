// Regression test for PRODUCTION-READINESS-100.md's "Known blockers" fix
// (2026-09-15): native-sys used to unconditionally define four Windows
// runtime shims (`__stack_chk_guard`, `__stack_chk_fail`, `___chkstk_ms`,
// `LdrRegisterDllNotification`) whose own doc comments said they were only
// needed because "MSVC's linker needs them resolved" — but they weren't
// gated to the MSVC target, so on a GNU/MinGW build they duplicated symbols
// MinGW's own runtime already provides. The conflicting `___chkstk_ms` (the
// Windows stack-probe function, invoked automatically by any function with
// a large enough stack frame — not just Zig code) crashed with
// STATUS_ACCESS_VIOLATION the moment any code ran on a `rayon` worker
// thread, even a trivial closure making zero calls into this crate. Fixed
// by gating those shims to `#[cfg(target_env = "msvc")]` in `src/lib.rs`.
//
// This test is the minimal reproduction that found it — bisected down from
// the real crash (the `pledge` CLI segfaulting on every invocation, and a
// plain `cargo test` on `pledgepack-core` crashing identically) through a
// series of ruled-out candidates (mimalloc, plain thread spawning, tokio,
// rquickjs — all fine in isolation) to this one: `rayon` + this crate
// linked into the same binary.

#[test]
fn find_imports_works_from_a_rayon_worker_thread() {
    use rayon::prelude::*;

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .expect("failed to build rayon pool");

    // A trivial closure with zero calls into this crate — on its own this
    // was enough to crash before the fix, since the conflicting symbols
    // broke stack probing process-wide, not just inside this crate's code.
    let sum: i32 = pool.install(|| (0..4).sum());
    assert_eq!(sum, 6);

    let sources: Vec<Vec<u8>> = (0..50)
        .map(|i| format!("import mod{i} from \"./mod{i}\";\nexport const x = {i};\n").into_bytes())
        .collect();

    let results: Vec<usize> = pool.install(|| {
        sources
            .par_iter()
            .map(|src| pledgepack_native_sys::find_imports(src).len())
            .collect()
    });

    assert_eq!(results.len(), 50);
    assert!(results.iter().all(|&n| n == 1));
}
