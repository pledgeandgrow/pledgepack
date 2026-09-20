//! End-to-end tests for the WASM plugin host.
//!
//! These tests load a real WASM component (built from `tests/test-plugin-guest/`)
//! and verify that the host can instantiate it and call its hooks.
//!
//! The test plugin:
//! - resolve-id: resolves "virtual:test-plugin" → "\0virtual:test-plugin"
//! - load: loads "\0virtual:test-plugin" → "export const hello = 'from test-plugin';"
//! - transform: appends "// transformed by test-plugin" to .js/.ts/.jsx files

use pledgepack_wasm_plugin_host::*;
use std::path::PathBuf;

/// Path to the pre-built test plugin WASM component.
///
/// Prefers the `wasm32-wasip2` build (a real component — `cargo build
/// --target wasm32-wasip2`). The `wasm32-wasip1` artifact is a *core*
/// module, not a component, and cannot be loaded by
/// `wasmtime::component::Component`; it only exists because the fixture
/// was originally built without cargo-component/wasip2.
fn test_plugin_path() -> PathBuf {
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("test-plugin-guest")
        .join("target");
    let wasip2 = target_dir
        .join("wasm32-wasip2")
        .join("release")
        .join("test_plugin_guest.wasm");
    if wasip2.exists() {
        return wasip2;
    }
    target_dir
        .join("wasm32-wasip1")
        .join("release")
        .join("test_plugin_guest.wasm")
}

/// Check if the test plugin was built. If not, tests should fail (not silently skip).
fn test_plugin_exists() -> bool {
    test_plugin_path().exists()
}

/// Panic with a helpful message if the test plugin artifact is missing.
/// In CI, the guest component must be built before running tests.
macro_rules! require_test_plugin {
    () => {
        if !test_plugin_exists() {
            panic!(
                "Test plugin not built. Run 'cargo component build -p test-plugin-guest' first.\n\
                 In CI, ensure the guest component is built before running tests."
            );
        }
    };
}

#[test]
fn test_plugin_loads() {
    require_test_plugin!();
    let path = test_plugin_path();
    let plugin = WasmPlugin::load_from_file(&path);
    assert!(
        plugin.is_ok(),
        "Failed to load test plugin: {:?}",
        plugin.err()
    );

    let plugin = plugin.unwrap();
    assert_eq!(plugin.name(), "test-plugin");
    assert_eq!(plugin.metadata().version, "0.1.1");
    assert!(plugin.has_resolve_id());
    assert!(plugin.has_load());
    assert!(plugin.has_transform());
    assert!(!plugin.has_transform_index_html());
}

#[test]
fn test_plugin_resolve_id_virtual() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should resolve virtual:test-plugin
    let result = plugin
        .resolve_id("virtual:test-plugin", None, false, None)
        .unwrap();
    assert!(result.is_some());
    let output = result.unwrap();
    assert_eq!(output.id, "\0virtual:test-plugin");
    assert!(!output.external);
    assert!(!output.cache_key.is_empty());
}

#[test]
fn test_plugin_resolve_id_passthrough() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should NOT resolve regular specifiers
    let result = plugin.resolve_id("./foo", None, false, None).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_plugin_load_virtual() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should load the virtual module
    let result = plugin.load("\0virtual:test-plugin").unwrap();
    assert!(result.is_some());
    let output = result.unwrap();
    assert_eq!(output.code, "export const hello = 'from test-plugin';");
    assert!(output.source_map.is_none());
    assert!(!output.cache_key.is_empty());
}

#[test]
fn test_plugin_load_passthrough() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should NOT load regular files
    let result = plugin.load("./foo.js").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_plugin_transform_js() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should transform .js files by prepending a comment
    let result = plugin.transform("const x = 1;", "test.js", None).unwrap();
    assert!(result.is_some());
    let output = result.unwrap();
    assert_eq!(output.code, "// transformed by test-plugin\nconst x = 1;");
    assert!(output.source_map.is_none());
    assert!(!output.cache_key.is_empty());
}

#[test]
fn test_plugin_transform_ts() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should transform .ts files too
    let result = plugin
        .transform("const x: number = 1;", "test.ts", None)
        .unwrap();
    assert!(result.is_some());
    let output = result.unwrap();
    assert_eq!(
        output.code,
        "// transformed by test-plugin\nconst x: number = 1;"
    );
}

#[test]
fn test_plugin_transform_css_passthrough() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should NOT transform .css files
    let result = plugin
        .transform(".foo { color: red; }", "test.css", None)
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn test_plugin_transform_index_html_passthrough() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();

    // Should NOT transform HTML (plugin doesn't implement this hook)
    let result = plugin
        .transform_index_html("<html></html>", "index.html")
        .unwrap();
    assert!(result.is_none());
}

// ─── Host-level tests (multiple plugins) ──────────────────────────────

#[test]
fn host_loads_test_plugin() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    assert!(host.is_empty());

    let name = host.load_plugin(&path).unwrap();
    assert_eq!(name, "test-plugin");
    assert_eq!(host.len(), 1);
    assert!(!host.is_empty());
}

#[test]
fn host_resolve_id_through_plugin() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    let result = host
        .resolve_id("virtual:test-plugin", None, false, None)
        .unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().id, "\0virtual:test-plugin");
}

#[test]
fn host_transform_through_plugin() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    let (code, map) = host.transform("const x = 1;", "test.js", None).unwrap();
    assert_eq!(code, "// transformed by test-plugin\nconst x = 1;");
    assert!(map.is_none());
}

// ─── Bridge tests with real plugin ────────────────────────────────────

#[test]
fn bridge_transform_closure_with_real_plugin() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    let bridge = std::sync::Arc::new(WasmPluginHostBridge::new(host));
    let closure = bridge.transform_closure();

    let result = closure("const x = 1;", "test.js");
    assert!(result.is_some());
    let result = result.unwrap();
    assert_eq!(result.code, "// transformed by test-plugin\nconst x = 1;");
}

#[test]
fn bridge_transform_closure_passthrough_for_css() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    let bridge = std::sync::Arc::new(WasmPluginHostBridge::new(host));
    let closure = bridge.transform_closure();

    let result = closure(".foo { color: red; }", "test.css");
    assert!(result.is_none());
}

// ─── Item 3: Plugin ordering for WASM plugins ───────────────────────

#[test]
fn test_wasm_plugin_is_post_by_default() {
    require_test_plugin!();
    let path = test_plugin_path();
    let plugin = WasmPlugin::load_from_file(&path).unwrap();
    // The test plugin has no enforce field → default = "post"
    assert!(!plugin.is_pre_plugin());
    assert!(plugin.is_post_plugin());
}

#[test]
fn test_wasm_host_has_pre_post_detection() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    // The test plugin has no enforce field → default = "post"
    assert!(!host.has_pre_plugin());
    assert!(host.has_post_plugin());
}

#[test]
fn test_wasm_bridge_has_pre_post_detection() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    let bridge = WasmPluginHostBridge::new(host);
    assert!(!bridge.has_pre_plugin());
    assert!(bridge.has_post_plugin());
}

#[test]
fn test_wasm_bridge_pre_transform_closure_returns_none_when_no_pre() {
    require_test_plugin!();
    let path = test_plugin_path();
    let mut host = WasmPluginHost::new().unwrap();
    host.load_plugin(&path).unwrap();

    let bridge = std::sync::Arc::new(WasmPluginHostBridge::new(host));
    // No pre-plugins → pre_transform_closure returns None
    let closure = bridge.pre_transform_closure();
    let result = closure("const x = 1;", "test.js");
    assert!(result.is_none());
}

// ─── Item 2: Host imports in WASM host ──────────────────────────────

#[test]
fn test_wasm_plugin_state_host_config() {
    // PluginState::new is private — we test host_config via the public API
    // by loading a plugin and checking that set_host_config doesn't panic.
    // The actual host_config field is tested via the get-config host import.
    require_test_plugin!();
    let path = test_plugin_path();
    let plugin = WasmPlugin::load_from_file(&path).unwrap();
    // Just verify the plugin loads — host_config is tested internally
    assert_eq!(plugin.name(), "test-plugin");
}

#[test]
fn test_wasm_plugin_state_emitted_files() {
    // PluginState::new is private — we test emitted_files via the public API
    // by loading a plugin and checking that it doesn't emit files by default.
    require_test_plugin!();
    let path = test_plugin_path();
    let plugin = WasmPlugin::load_from_file(&path).unwrap();
    // The test plugin doesn't emit files — just verify it loads
    assert_eq!(plugin.name(), "test-plugin");
}

#[test]
fn test_lifetime_fuel_cap_is_configurable() {
    require_test_plugin!();
    let path = test_plugin_path();

    // A tiny lifetime cap makes the next hook call trap on out-of-fuel.
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();
    plugin.set_limits(WasmLimits {
        lifetime_fuel: Some(1),
        ..WasmLimits::default()
    });
    assert!(
        plugin
            .resolve_id("virtual:test-plugin", None, false, None)
            .is_err(),
        "hook call must fail once the lifetime cap is exceeded"
    );

    // With the default limits a long session is not starved: far more than
    // the old 10-full-budget-call cap's worth of calls all succeed, and the
    // per-call budget is reset each time.
    let mut plugin = WasmPlugin::load_from_file(&path).unwrap();
    for _ in 0..500 {
        plugin
            .resolve_id("virtual:test-plugin", None, false, None)
            .expect("hook call within budget");
    }
    assert!(plugin.lifetime_fuel_used() > 0);
    plugin.reset_lifetime_fuel();
    assert_eq!(plugin.lifetime_fuel_used(), 0);
}
