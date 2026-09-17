fn main() {
    // CARGO_MANIFEST_DIR = <root>/native-sys
    // zig-out is at <root>/zig-out
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set — build.rs must be invoked by cargo");
    let root = std::path::Path::new(&manifest_dir)
        .parent()
        .expect("manifest dir has no parent — invalid directory structure");
    let lib_dir = root.join("zig-out").join("lib");

    // Invoke `zig build` if the static library artifact is missing or stale.
    let lib_path = lib_dir.join("libpledge_native.a");
    if !lib_path.exists() {
        let zig_executable = std::env::var("ZIG_EXECUTABLE").unwrap_or_else(|_| "zig".to_string());
        let status = std::process::Command::new(&zig_executable)
            .arg("build")
            .current_dir(root)
            .status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("[pledge-native-sys] zig build completed successfully");
            }
            Ok(s) => panic!("zig build failed with status: {}", s),
            Err(e) => panic!(
                "Failed to run 'zig build': {}. Is Zig installed and on PATH?",
                e
            ),
        }
    }

    // Use CARGO_CFG_TARGET_OS (set by cargo for the TARGET, not host)
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // ...and CARGO_CFG_TARGET_ENV to distinguish windows-msvc from windows-gnu
    // (both report target_os == "windows").
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    // On Windows, `zig build` always names its static-library output
    // `pledge_native.lib` (Zig's own convention on this OS, independent of
    // target ABI) rather than the `lib*.a` name this build script otherwise
    // expects (and uses as its "already built?" marker above). rustc's
    // native-library search on `-windows-gnu` targets only recognizes the
    // `lib*.a` name, so without this the link step fails with "could not
    // find native static library `pledge_native`" even though the file is
    // right there under a different name. Normalize by copying rather than
    // renaming, so re-running `zig build` (which only regenerates the
    // `.lib`) doesn't leave a stale copy behind silently — same rationale as
    // the macOS archive-alignment fix below.
    if target_os == "windows" && target_env == "gnu" {
        let msvc_style = lib_dir.join("pledge_native.lib");
        let gnu_style = lib_dir.join("libpledge_native.a");
        if msvc_style.exists()
            && let Err(e) = std::fs::copy(&msvc_style, &gnu_style)
        {
            eprintln!(
                "[pledge-native-sys] WARNING: failed to copy {} -> {}: {e}",
                msvc_style.display(),
                gnu_style.display()
            );
        }
    }

    // Force re-run when target changes
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");

    // On macOS, Zig's archive format is not 8-byte aligned, which Apple's ld rejects.
    // We produce a .o file from Zig, then create a NEW archive with proper alignment
    // using `ar rcs`. This is necessary because:
    // 1. cargo:rustc-link-arg does NOT propagate through transitive dependencies
    //    (pledgepack-cli -> pledgepack-core -> pledgepack-native-sys)
    // 2. Only cargo:rustc-link-search + cargo:rustc-link-lib propagate transitively
    // 3. So we must fix the .a archive rather than trying to link the .o directly
    if target_os == "macos" {
        let obj_path = root.join("zig-out").join("pledge_native.o");
        let lib_path = lib_dir.join("libpledge_native.a");

        eprintln!(
            "[pledge-native-sys] target_os=macos, obj={} exists={}",
            obj_path.display(),
            obj_path.exists()
        );
        eprintln!(
            "[pledge-native-sys] lib={} exists={}",
            lib_path.display(),
            lib_path.exists()
        );

        if obj_path.exists() {
            // Create a new archive from the .o file with proper alignment
            let _ = std::fs::remove_file(&lib_path);
            let status = std::process::Command::new("ar")
                .arg("rcs")
                .arg(&lib_path)
                .arg(&obj_path)
                .status();
            match status {
                Ok(s) if s.success() => {
                    eprintln!("[pledge-native-sys] created new archive from .o file");
                }
                _ => {
                    eprintln!("[pledge-native-sys] WARNING: ar rcs failed, using original archive");
                }
            }
        } else {
            eprintln!("[pledge-native-sys] WARNING: .o file not found, using original archive");
        }
    }

    // These directives propagate through transitive dependencies
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=pledge_native");

    // Link Windows libraries needed by Zig runtime
    if target_os == "windows" {
        println!("cargo:rustc-link-lib=dylib=ntdll");
    }

    // Tell cargo to rerun if Zig source changes
    let zig_src = root.join("native-sys").join("zig");
    println!(
        "cargo:rerun-if-changed={}",
        zig_src.join("lib.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        zig_src.join("io.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        zig_src.join("graph.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        zig_src.join("simd.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        zig_src.join("bench.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("build.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("build.zig.zon").display()
    );
}
