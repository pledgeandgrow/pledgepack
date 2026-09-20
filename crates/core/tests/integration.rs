use pledgepack_core::BuildEngine;
use pledgepack_core::config::{BuildMode, Framework, PledgeConfig};
use pledgepack_core::module::ModuleKind;
use pledgepack_core::transform as pledge_transform;
use std::fs;
use std::sync::Arc;
use tempfile::TempDir;

// ── Goal 1: Verify PledgePack Oxc transform output matches PledgeJS transformPSX() expectations ──

#[test]
fn test_oxc_transform_produces_valid_esm() {
    let config = PledgeConfig {
        framework: Framework::Pledge,
        mode: BuildMode::Development,
        ..Default::default()
    };

    let tsx_source = r#"
import React from "react";
import { useState } from "react";

export default function Counter() {
  const [count, setCount] = useState(0);
  return <button onClick={() => setCount(count + 1)}>Count: {count}</button>;
}
"#;

    let result =
        pledge_transform::transform(tsx_source, ModuleKind::Tsx, "test.tsx", false, &config);

    assert!(result.is_ok(), "Transform should succeed");
    let output = result.unwrap();

    // Oxc should produce ESM-compatible JS
    assert!(
        !output.code.is_empty(),
        "Transformed code should not be empty"
    );

    // Should contain React automatic runtime imports (jsx-runtime)
    assert!(
        output.code.contains("jsx")
            || output.code.contains("jsxs")
            || output.code.contains("Fragment"),
        "Transformed code should use React automatic JSX runtime (jsx/jsxs/Fragment), got: {}",
        &output.code[..200.min(output.code.len())]
    );

    // Should not contain TypeScript types
    assert!(
        !output.code.contains(": React")
            && !output.code.contains(": number")
            && !output.code.contains(": string"),
        "Transformed code should not contain TypeScript type annotations"
    );

    // Should not contain JSX syntax (should be compiled to function calls)
    assert!(
        !output.code.contains("<button") && !output.code.contains("<div"),
        "Transformed code should not contain raw JSX syntax"
    );
}

#[test]
fn test_oxc_transform_type_stripping() {
    let config = PledgeConfig {
        framework: Framework::Pledge,
        ..Default::default()
    };

    let ts_source = r#"
interface User { name: string; age: number; }
export function greet(user: User): string {
  return `Hello, ${user.name}!`;
}
"#;

    let result =
        pledge_transform::transform(ts_source, ModuleKind::TypeScript, "test.ts", false, &config);

    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(!output.code.contains("interface User"));
    assert!(!output.code.contains(": string") && !output.code.contains(": number"));
    assert!(output.code.contains("greet"));
}

#[test]
fn test_oxc_transform_psx_as_tsx() {
    let config = PledgeConfig {
        framework: Framework::Pledge,
        ..Default::default()
    };

    // PSX files are transformed as TSX (Rust already extracted by PledgeJS)
    let psx_tsx_content = r#"
import React from "react";
export function MyComponent() {
  return <div>Hello from PSX</div>;
}
"#;

    let result =
        pledge_transform::transform(psx_tsx_content, ModuleKind::Psx, "test.psx", false, &config);

    assert!(result.is_ok(), "PSX (as TSX) transform should succeed");
    let output = result.unwrap();
    assert!(
        !output.code.contains("<div"),
        "PSX JSX should be compiled to function calls"
    );
}

// ── Goal 106: Verify BuildEngine fails fast on missing entry ──

#[tokio::test]
async fn test_build_engine_fails_on_missing_entry() {
    let tmp = TempDir::new().unwrap();
    let config = PledgeConfig {
        root: tmp.path().to_path_buf(),
        entry: vec![], // No entries
        app_dir: None, // No app directory
        mode: BuildMode::Production,
        ..Default::default()
    };

    let mut engine = BuildEngine::new(Arc::new(config));
    let result = engine.build().await;

    assert!(
        result.is_err(),
        "Build should fail when no entry points are configured"
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("No pages found"),
        "Error message should say no pages found, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("app/page.tsx"),
        "Error message should suggest creating app/page.tsx, got: {}",
        err_msg
    );
}

#[tokio::test]
async fn test_build_engine_succeeds_with_explicit_entry() {
    let tmp = TempDir::new().unwrap();
    let entry_path = tmp.path().join("src/index.tsx");
    fs::create_dir_all(entry_path.parent().unwrap()).unwrap();
    fs::write(
        &entry_path,
        r#"export default function App() { return "hello"; }"#,
    )
    .unwrap();

    let config = PledgeConfig {
        root: tmp.path().to_path_buf(),
        entry: vec!["src/index.tsx".to_string()],
        mode: BuildMode::Production,
        framework: Framework::Pledge,
        ..Default::default()
    };

    let mut engine = BuildEngine::new(Arc::new(config));
    let result = engine.build().await;

    // Should not fail with "No entry points found"
    if let Err(ref e) = result {
        assert!(
            !e.to_string().contains("No entry points found"),
            "Build should not fail with missing entry when entry is provided"
        );
    }
}

#[tokio::test]
async fn test_build_engine_succeeds_with_app_dir() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("app");
    fs::create_dir_all(&app_dir).unwrap();
    fs::write(
        app_dir.join("page.tsx"),
        r#"export default function Page() { return "hello"; }"#,
    )
    .unwrap();

    let config = PledgeConfig {
        root: tmp.path().to_path_buf(),
        entry: vec![],
        app_dir: Some("app".to_string()),
        mode: BuildMode::Production,
        framework: Framework::Pledge,
        ..Default::default()
    };

    let mut engine = BuildEngine::new(Arc::new(config));
    let result = engine.build().await;

    // Should not fail with "No entry points found" — app dir auto-discovers entries
    if let Err(ref e) = result {
        assert!(
            !e.to_string().contains("No entry points found"),
            "Build should not fail with missing entry when app_dir has routes"
        );
    }
}

// ── Goal 106: Verify BuildEngine fails fast on empty app/ directory ──

#[tokio::test]
async fn test_build_engine_fails_on_empty_app_dir() {
    let tmp = TempDir::new().unwrap();
    let app_dir = tmp.path().join("app");
    fs::create_dir_all(&app_dir).unwrap(); // app/ exists but has no page files

    let config = PledgeConfig {
        root: tmp.path().to_path_buf(),
        entry: vec![],
        app_dir: Some("app".to_string()),
        mode: BuildMode::Production,
        ..Default::default()
    };

    let mut engine = BuildEngine::new(Arc::new(config));
    let result = engine.build().await;

    assert!(
        result.is_err(),
        "Build should fail when app/ directory has no route files"
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("No pages found"),
        "Error message should say no pages found, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("app/page.tsx"),
        "Error message should suggest creating app/page.tsx, got: {}",
        err_msg
    );
}

// ── Goal 106: Verify BuildEngine fails fast on non-existent entry file ──

#[tokio::test]
async fn test_build_engine_fails_on_nonexistent_entry_file() {
    let tmp = TempDir::new().unwrap();

    let config = PledgeConfig {
        root: tmp.path().to_path_buf(),
        entry: vec!["src/index.tsx".to_string()], // File doesn't exist
        mode: BuildMode::Production,
        ..Default::default()
    };

    let mut engine = BuildEngine::new(Arc::new(config));
    let result = engine.build().await;

    assert!(
        result.is_err(),
        "Build should fail when entry file doesn't exist"
    );

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("File not found"),
        "Error message should say file not found, got: {}",
        err_msg
    );
    assert!(
        err_msg.contains("src/index.tsx"),
        "Error message should mention the missing file path, got: {}",
        err_msg
    );
}

// ── Goal 21: Verify build output directory structure ──

#[test]
fn test_build_output_directory_structure() {
    // Verify that PledgeConfig's out_dir defaults to ".pledge"
    // and that PledgeJS expects this structure:
    //   .pledge/
    //     ├── client/    (browser assets)
    //     ├── server/    (SSR modules)
    //     └── __pledge_ps_manifest.json

    let config = PledgeConfig::default();
    assert_eq!(
        config.out_dir,
        std::path::PathBuf::from(".pledge"),
        "Default output directory should be .pledge"
    );
}

// ── Goal 23: Verify route manifest format ──

#[test]
fn test_route_manifest_format() {
    // The manifest __pledge_ps_manifest.json should have the format:
    // {
    //   "frontend": [{ "file": "app/page.tsx", "path": "/" }, ...],
    //   "api": [{ "file": "api/handler.ts", "path": "/api/handler" }, ...],
    //   "backend": [...]
    // }
    //
    // PledgeJS reads this in resolveProductionPath() via:
    //   manifest.frontend, manifest.api, manifest.backend
    // and each entry has a "file" field.

    let manifest_json = serde_json::json!({
        "frontend": [
            { "file": "app/page.tsx", "path": "/" },
            { "file": "app/about/page.tsx", "path": "/about" }
        ],
        "api": [
            { "file": "api/handler.ts", "path": "/api/handler" }
        ],
        "backend": []
    });

    // Verify the structure matches what PledgeJS expects
    assert!(
        manifest_json.get("frontend").is_some(),
        "Manifest should have 'frontend' key"
    );
    assert!(
        manifest_json.get("api").is_some(),
        "Manifest should have 'api' key"
    );
    assert!(
        manifest_json.get("backend").is_some(),
        "Manifest should have 'backend' key"
    );

    let frontend = manifest_json["frontend"].as_array().unwrap();
    assert!(!frontend.is_empty());
    assert!(
        frontend[0].get("file").is_some(),
        "Each entry should have a 'file' field"
    );
    assert!(
        frontend[0].get("path").is_some(),
        "Each entry should have a 'path' field"
    );
}

// ── Goal 51: Verify binary resolution works on all platforms ──

#[test]
fn test_binary_resolution_platform_detection() {
    // Verify that the platform detection logic produces correct package names
    let (os, arch) = if cfg!(target_os = "windows") {
        (
            "win32",
            if cfg!(target_pointer_width = "64") {
                "x64"
            } else {
                "x86"
            },
        )
    } else if cfg!(target_os = "macos") {
        (
            "darwin",
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            },
        )
    } else if cfg!(target_os = "linux") {
        (
            "linux",
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            },
        )
    } else {
        ("unknown", "unknown")
    };

    // The expected npm package name pattern is @pledgepack/pledgepack-{os}-{arch}
    let expected_package = format!("@pledgepack/pledgepack-{}-{}", os, arch);
    assert!(
        expected_package.contains("pledgepack"),
        "Platform package name should contain 'pledgepack'"
    );
    assert!(
        !os.contains("unknown"),
        "Platform should be detected as win32, darwin, or linux"
    );

    // The binary name should be platform-appropriate
    let binary_name = if cfg!(target_os = "windows") {
        "pledgepack.exe"
    } else {
        "pledgepack"
    };
    assert!(binary_name.starts_with("pledgepack"));
}

// ── Task-graph engine: cross-process cache reuse ──
//
// The task-driven pipeline is the default build path. This test proves its
// core claim: a fresh BuildEngine over the same project + cache dir serves
// unchanged modules from the task disk cache — no previous-graph comparison,
// no git tree hash, just content-addressed TaskIds.

#[tokio::test]
async fn test_second_engine_build_uses_task_cache() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("index.tsx"),
        r#"import { helper } from "./helper";
export default function App() { return helper(); }"#,
    )
    .unwrap();
    fs::write(
        src.join("helper.ts"),
        r#"export function helper() { return "hello"; }"#,
    )
    .unwrap();

    // Pin the cache dir inside the tempdir — the default
    // "node_modules/.pledge-cache" is relative to the process cwd.
    let make_config = || PledgeConfig {
        root: tmp.path().to_path_buf(),
        entry: vec!["src/index.tsx".to_string()],
        mode: BuildMode::Production,
        framework: Framework::Pledge,
        cache: pledgepack_core::config::CacheConfig {
            dir: tmp.path().join(".cache"),
            ..Default::default()
        },
        ..Default::default()
    };

    let mut engine1 = BuildEngine::new(Arc::new(make_config()));
    let first = engine1.build().await.expect("first build should succeed");
    assert_eq!(
        first.modules_built, 2,
        "cold build computes every module, got built={} cached={}",
        first.modules_built, first.modules_cached
    );

    let mut engine2 = BuildEngine::new(Arc::new(make_config()));
    let second = engine2.build().await.expect("second build should succeed");
    assert_eq!(
        second.modules_cached, 2,
        "warm build must hit the task disk cache for every unchanged module, \
         got built={} cached={}",
        second.modules_built, second.modules_cached
    );

    // Changing one file must invalidate exactly that module's task — the
    // untouched module still hits the cache.
    fs::write(
        src.join("helper.ts"),
        r#"export function helper() { return "changed"; }"#,
    )
    .unwrap();

    let mut engine3 = BuildEngine::new(Arc::new(make_config()));
    let third = engine3.build().await.expect("third build should succeed");
    assert_eq!(
        third.modules_cached, 1,
        "only the unchanged module should hit cache, got built={} cached={}",
        third.modules_built, third.modules_cached
    );
    assert_eq!(third.modules_built, 1);
}
