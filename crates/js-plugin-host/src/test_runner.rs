// Test Runner — Vitest-compatible test execution via rquickjs (QuickJS)
//
// Provides:
//   - describe/it/test API registration
//   - expect() assertion library with snapshot support
//   - beforeAll/beforeEach/afterAll/afterEach hooks
//   - Real JS execution via QuickJS (rquickjs bindings)
//   - Pass/fail/skip reporting
//   - Code coverage collection
//   - Setup files support
//   - Test environment support (node, jsdom, happy-dom)
//   - Globals mode (describe/it/expect without imports)
//   - Test isolation modes (file, pool, none)
//   - UI mode (HTML report generation)

use anyhow::Result;
use rquickjs::prelude::{Func, Opt, Rest};
use rquickjs::{Context, Ctx, Object, Runtime};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Coverage data for a single file
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CoverageEntry {
    pub file: String,
    pub lines_total: usize,
    pub lines_covered: usize,
    pub functions_total: usize,
    pub functions_covered: usize,
    pub branches_total: usize,
    pub branches_covered: usize,
}

/// Coverage report aggregating all files
#[derive(Debug, Clone, Default)]
pub struct CoverageReport {
    pub entries: Vec<CoverageEntry>,
}

impl CoverageReport {
    pub fn summary(&self) -> (usize, usize, f64) {
        let total_lines: usize = self.entries.iter().map(|e| e.lines_total).sum();
        let covered_lines: usize = self.entries.iter().map(|e| e.lines_covered).sum();
        let pct = if total_lines > 0 {
            (covered_lines as f64 / total_lines as f64) * 100.0
        } else {
            0.0
        };
        (total_lines, covered_lines, pct)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.entries).unwrap_or_else(|_| "[]".to_string())
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str("File\t\t\tLines\tFuncs\tBranches\n");
        out.push_str("----\t\t\t-----\t-----\t--------\n");
        for e in &self.entries {
            out.push_str(&format!(
                "{}\t\t{}/{}\t{}/{}\t{}/{}\n",
                e.file,
                e.lines_covered,
                e.lines_total,
                e.functions_covered,
                e.functions_total,
                e.branches_covered,
                e.branches_total,
            ));
        }
        let (total, covered, pct) = self.summary();
        out.push_str(&format!(
            "\nAll files\t{}/{}\t{:.1}%\n",
            covered, total, pct
        ));
        out
    }

    pub fn to_lcov(&self) -> String {
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(&format!("SF:{}\n", e.file));
            out.push_str(&format!("LF:{}\n", e.lines_total));
            out.push_str(&format!("LH:{}\n", e.lines_covered));
            out.push_str("end_of_record\n");
        }
        out
    }

    pub fn to_html(&self) -> String {
        let (total, covered, pct) = self.summary();
        let mut rows = String::new();
        for e in &self.entries {
            let line_pct = if e.lines_total > 0 {
                (e.lines_covered as f64 / e.lines_total as f64) * 100.0
            } else {
                0.0
            };
            let color = if line_pct >= 80.0 {
                "#4caf50"
            } else if line_pct >= 50.0 {
                "#ff9800"
            } else {
                "#f44336"
            };
            rows.push_str(&format!(
                "<tr><td>{}</td><td style='color:{}'>{:.1}%</td><td>{}/{}</td><td>{}/{}</td></tr>",
                e.file,
                color,
                line_pct,
                e.lines_covered,
                e.lines_total,
                e.functions_covered,
                e.functions_total,
            ));
        }
        format!(
            r#"<!DOCTYPE html>
<html><head><meta charset="UTF-8"><title>Coverage Report</title>
<style>body{{font-family:monospace;background:#1a1a1a;color:#e0e0e0;padding:2rem;}}
table{{border-collapse:collapse;width:100%;}}th,td{{border:1px solid #333;padding:0.5rem;text-align:left;}}
th{{background:#333;}}.summary{{margin-bottom:1rem;font-size:1.2rem;}}</style></head>
<body><div class="summary">Coverage: {}/{} lines ({:.1}%)</div>
<table><tr><th>File</th><th>Lines</th><th>Functions</th><th>Branches</th></tr>
{}</table></body></html>"#,
            covered, total, pct, rows
        )
    }
}

/// Snapshot store for toMatchSnapshot/toMatchInlineSnapshot
#[derive(Debug, Clone, Default)]
pub struct SnapshotStore {
    pub snapshots: std::collections::HashMap<String, String>,
    pub new_snapshots: Vec<(String, String)>,
    pub updated: usize,
    pub added: usize,
}

impl SnapshotStore {
    pub fn load(snapshot_dir: &Path, test_file: &Path) -> Self {
        let mut store = Self::default();
        let snapshot_file = snapshot_dir
            .join(test_file.file_name().unwrap_or_default())
            .with_extension("snap");
        if let Ok(content) = std::fs::read_to_string(&snapshot_file) {
            for line in content.lines() {
                if let Some(colon_pos) = line.find("\t") {
                    let key = &line[..colon_pos];
                    let val = &line[colon_pos + 1..];
                    store.snapshots.insert(key.to_string(), val.to_string());
                }
            }
        }
        store
    }

    pub fn save(&self, snapshot_dir: &Path, test_file: &Path) {
        std::fs::create_dir_all(snapshot_dir).ok();
        let snapshot_file = snapshot_dir
            .join(test_file.file_name().unwrap_or_default())
            .with_extension("snap");
        let mut content = String::new();
        for (key, val) in &self.snapshots {
            content.push_str(&format!("{}\t{}\n", key, val));
        }
        std::fs::write(&snapshot_file, content).ok();
    }

    pub fn compare(&mut self, key: &str, value: &str, update: bool) -> Result<bool> {
        if update || !self.snapshots.contains_key(key) {
            if !self.snapshots.contains_key(key) {
                self.added += 1;
            } else {
                self.updated += 1;
            }
            self.snapshots.insert(key.to_string(), value.to_string());
            self.new_snapshots
                .push((key.to_string(), value.to_string()));
            Ok(true)
        } else {
            let existing = self.snapshots.get(key).unwrap();
            Ok(existing == value)
        }
    }
}

/// HTML report for UI mode
pub fn generate_html_report(summaries: &[(String, TestSummary)]) -> String {
    let mut test_rows = String::new();
    let mut total_passed = 0;
    let mut total_failed = 0;
    let mut total_skipped = 0;

    for (file, summary) in summaries {
        total_passed += summary.passed;
        total_failed += summary.failed;
        total_skipped += summary.skipped;

        for result in &summary.results {
            let (icon, color) = match result.status {
                TestStatus::Passed => ("✓", "#4caf50"),
                TestStatus::Failed => ("✗", "#f44336"),
                TestStatus::Skipped => ("○", "#9e9e9e"),
            };
            let error_html = result
                .error
                .as_ref()
                .map(|e| {
                    format!(
                        "<div style='color:#f44336;padding-left:2rem;font-size:0.85rem;'>{}</div>",
                        e.replace('<', "&lt;").replace('>', "&gt;")
                    )
                })
                .unwrap_or_default();
            test_rows.push_str(&format!(
                "<tr><td style='color:{}'>{} {}</td><td style='color:#888'>{}</td><td style='color:#888'>{}ms</td></tr>{}",
                color, icon, result.name, file, result.duration_ms, error_html
            ));
        }
    }

    let status_color = if total_failed > 0 {
        "#f44336"
    } else {
        "#4caf50"
    };
    let status_text = if total_failed > 0 { "FAILED" } else { "PASSED" };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Pledge Test Results</title>
<style>
  body {{ font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #1a1a1a; color: #e0e0e0; padding: 2rem; }}
  .header {{ display: flex; align-items: center; gap: 1rem; margin-bottom: 2rem; }}
  .status {{ font-size: 1.5rem; font-weight: 600; }}
  .summary {{ display: flex; gap: 2rem; margin-bottom: 2rem; }}
  .summary-item {{ padding: 0.5rem 1rem; border-radius: 4px; }}
  table {{ border-collapse: collapse; width: 100%; }}
  th, td {{ border: 1px solid #333; padding: 0.5rem; text-align: left; }}
  th {{ background: #333; }}
</style></head>
<body>
  <div class="header">
    <span class="status" style="color:{}">{} {}</span>
  </div>
  <div class="summary">
    <span class="summary-item" style="background:#1b3a1b;color:#4caf50;">{} passed</span>
    <span class="summary-item" style="background:#3a1b1b;color:#f44336;">{} failed</span>
    <span class="summary-item" style="background:#333;color:#9e9e9e;">{} skipped</span>
  </div>
  <table><tr><th>Test</th><th>File</th><th>Duration</th></tr>
  {}
  </table>
</body></html>"#,
        status_color, status_text, "", total_passed, total_failed, total_skipped, test_rows
    )
}

/// Resource limits applied to the QuickJS runtime that evaluates a test file.
///
/// Test files are arbitrary user code, but their runtimes used to be created
/// with `Runtime::new()` and no limits at all: `while (true) {}` hung the
/// test run forever and a runaway allocation could exhaust the host.
#[derive(Debug, Clone, Copy)]
pub struct SandboxLimits {
    /// Heap cap in bytes (0 = unlimited).
    pub memory_limit: usize,
    /// Wall-clock budget for evaluating the test file (zero = unlimited).
    pub timeout: Duration,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            memory_limit: 256 * 1024 * 1024,
            timeout: Duration::from_secs(60),
        }
    }
}

/// Handle used to disarm the wall-clock guard once test evaluation is done
/// (so collecting results is not interrupted by an already-expired deadline).
struct DeadlineGuard {
    armed: Arc<AtomicBool>,
}

impl DeadlineGuard {
    fn disarm(&self) {
        self.armed.store(false, Ordering::Relaxed);
    }
}

impl SandboxLimits {
    /// Create a runtime with these limits applied.
    fn new_runtime(&self) -> Result<(Runtime, DeadlineGuard)> {
        let runtime = Runtime::new()?;
        crate::apply_memory_limit(&runtime, self.memory_limit);
        let armed = Arc::new(AtomicBool::new(true));
        if !self.timeout.is_zero() {
            let deadline = Instant::now() + self.timeout;
            let armed = armed.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || {
                armed.load(Ordering::Relaxed) && Instant::now() >= deadline
            })));
        }
        Ok((runtime, DeadlineGuard { armed }))
    }
}

/// Run a test file with full configuration support
pub fn run_test_file_with_config(
    file_path: &Path,
    config: &pledgepack_core::TestConfig,
    root: &Path,
    project_config: &pledgepack_core::PledgeConfig,
) -> Result<TestSummary> {
    run_test_file_with_config_and_limits(
        file_path,
        config,
        root,
        project_config,
        &SandboxLimits::default(),
    )
}

/// [`run_test_file_with_config`] with explicit sandbox limits.
pub fn run_test_file_with_config_and_limits(
    file_path: &Path,
    config: &pledgepack_core::TestConfig,
    root: &Path,
    project_config: &pledgepack_core::PledgeConfig,
    limits: &SandboxLimits,
) -> Result<TestSummary> {
    let source = std::fs::read_to_string(file_path)?;
    let (runtime, guard) = limits.new_runtime()?;
    let context = Context::full(&runtime)?;

    let mut results = Vec::new();
    let mut errors = Vec::new();

    // Set up snapshot support
    let snapshot_store = Arc::new(Mutex::new(SnapshotStore::load(
        &root.join(&config.snapshot_dir),
        file_path,
    )));

    // Set up coverage tracking if enabled
    let coverage_data = Arc::new(Mutex::new(Vec::<(String, usize, usize)>::new()));

    // Bundle the test file with the project modules it imports (relative
    // imports, aliases, TS/JSX, `vitest` globals) instead of deleting imports.
    let _ = &source;
    let js_source = crate::test_bundle::bundle_test_file(file_path, project_config)?;

    // Everything runs inside the context closure
    let results_json: String = context.with(|ctx| {
        // Set up the test harness (describe, it, test, expect, hooks)
        setup_test_harness(&ctx);

        // Inject console.log
        setup_console(&ctx);

        // Inject a simple module system (stub require/import)
        setup_module_shim(&ctx, file_path);

        // Set up test environment (jsdom/happy-dom shims)
        setup_test_environment(&ctx, &config.environment);

        // Run setup files before the test file
        for setup_file in &config.setup_files {
            let setup_path = root.join(setup_file);
            if setup_path.exists()
                && let Ok(setup_js) =
                    crate::test_bundle::bundle_test_file(&setup_path, project_config)
            {
                let _ = ctx.eval::<(), _>(setup_js.as_str());
            }
        }

        // Set up snapshot API
        setup_snapshot_api(&ctx, &snapshot_store, config.update_snapshots);

        // Set up coverage tracking if enabled
        if config.coverage {
            setup_coverage_tracking(&ctx, &coverage_data, file_path);
        }

        // Evaluate the test file
        if let Err(e) = ctx.eval::<(), _>(js_source.as_str()) {
            errors.push(format!("File evaluation error: {}", e));
        }
        guard.disarm();

        // Collect test results from the global __pledge_test_results array
        ctx.eval::<String, _>(
            r#"JSON.stringify((typeof __pledge_test_results !== 'undefined') ? __pledge_test_results : [])"#
        ).unwrap_or_else(|_| "[]".to_string())
    });

    // Parse results JSON
    if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&results_json)
        && let Some(tests) = arr.as_array()
    {
        for test in tests {
            let name = test
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let suite = test
                .get("suite")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status_str = test
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("skipped");
            let error = test
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let duration = test.get("duration").and_then(|v| v.as_u64()).unwrap_or(0) as u128;

            let status = match status_str {
                "passed" => TestStatus::Passed,
                "failed" => TestStatus::Failed,
                _ => TestStatus::Skipped,
            };

            results.push(TestResult {
                name,
                suite,
                status,
                error,
                duration_ms: duration,
            });
        }
    }

    // If there were eval errors and no results, add them as failed tests
    for err in errors {
        results.push(TestResult {
            name: file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string(),
            suite: String::new(),
            status: TestStatus::Failed,
            error: Some(err),
            duration_ms: 0,
        });
    }

    // Save snapshots if any were added or updated
    {
        let store = snapshot_store.lock().unwrap();
        if store.added > 0 || store.updated > 0 {
            store.save(&root.join(&config.snapshot_dir), file_path);
        }
    }

    // Build summary
    let passed = results
        .iter()
        .filter(|r| r.status == TestStatus::Passed)
        .count();
    let failed = results
        .iter()
        .filter(|r| r.status == TestStatus::Failed)
        .count();
    let skipped = results
        .iter()
        .filter(|r| r.status == TestStatus::Skipped)
        .count();

    Ok(TestSummary {
        total: results.len(),
        passed,
        failed,
        skipped,
        duration_ms: 0,
        results,
    })
}

/// Test result for a single test case
#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub suite: String,
    pub status: TestStatus,
    pub error: Option<String>,
    pub duration_ms: u128,
}

/// Status of a test case
#[derive(Debug, Clone, PartialEq)]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
}

/// Summary of a test run
#[derive(Debug, Clone)]
pub struct TestSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub duration_ms: u128,
    pub results: Vec<TestResult>,
}

/// Run a test file in the QuickJS runtime (legacy, no config)
pub fn run_test_file(file_path: &Path) -> Result<TestSummary> {
    run_test_file_with_limits(file_path, &SandboxLimits::default())
}

/// [`run_test_file`] with explicit sandbox limits.
pub fn run_test_file_with_limits(file_path: &Path, limits: &SandboxLimits) -> Result<TestSummary> {
    let source = std::fs::read_to_string(file_path)?;
    let (runtime, guard) = limits.new_runtime()?;
    let context = Context::full(&runtime)?;

    let mut results = Vec::new();
    let mut errors = Vec::new();

    let js_source = strip_typescript(
        &source,
        file_path,
        &pledgepack_core::PledgeConfig::default(),
    )?;

    let results_json: String = context.with(|ctx| {
        // Set up the test harness (describe, it, test, expect, hooks)
        setup_test_harness(&ctx);

        // Inject console.log
        setup_console(&ctx);

        // Inject a simple module system (stub require/import)
        setup_module_shim(&ctx, file_path);

        // Evaluate the test file
        if let Err(e) = ctx.eval::<(), _>(js_source.as_str()) {
            errors.push(format!("File evaluation error: {}", e));
        }
        guard.disarm();

        // Collect test results
        ctx.eval::<String, _>(
            r#"JSON.stringify((typeof __pledge_test_results !== 'undefined') ? __pledge_test_results : [])"#
        ).unwrap_or_else(|_| "[]".to_string())
    });

    // Parse results JSON
    if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&results_json)
        && let Some(tests) = arr.as_array()
    {
        for test in tests {
            let name = test
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let suite = test
                .get("suite")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status_str = test
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("skipped");
            let error = test
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let duration = test.get("duration").and_then(|v| v.as_u64()).unwrap_or(0) as u128;

            let status = match status_str {
                "passed" => TestStatus::Passed,
                "failed" => TestStatus::Failed,
                _ => TestStatus::Skipped,
            };

            results.push(TestResult {
                name,
                suite,
                status,
                error,
                duration_ms: duration,
            });
        }
    }

    // If there were eval errors and no results, add them as failed tests
    for err in errors {
        results.push(TestResult {
            name: file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string(),
            suite: String::new(),
            status: TestStatus::Failed,
            error: Some(err),
            duration_ms: 0,
        });
    }

    // Build summary
    let passed = results
        .iter()
        .filter(|r| r.status == TestStatus::Passed)
        .count();
    let failed = results
        .iter()
        .filter(|r| r.status == TestStatus::Failed)
        .count();
    let skipped = results
        .iter()
        .filter(|r| r.status == TestStatus::Skipped)
        .count();

    Ok(TestSummary {
        total: results.len(),
        passed,
        failed,
        skipped,
        duration_ms: 0,
        results,
    })
}

/// Set up the test harness in the JS context
fn setup_test_harness(ctx: &Ctx) {
    let harness_code = r#"
        var __pledge_test_results = [];
        var __pledge_current_suite = "";
        var __pledge_before_all = [];
        var __pledge_before_each = [];
        var __pledge_after_each = [];
        var __pledge_after_all = [];

        function describe(name, fn) {
            var prevSuite = __pledge_current_suite;
            __pledge_current_suite = name;
            try {
                fn();
            } catch(e) {
                __pledge_test_results.push({
                    name: name,
                    suite: "",
                    status: "failed",
                    error: "describe block error: " + e.message,
                    duration: 0
                });
            }
            __pledge_current_suite = prevSuite;
        }

        function it(name, fn) {
            __run_test(name, fn, false);
        }

        function test(name, fn) {
            __run_test(name, fn, false);
        }

        it.skip = function(name, fn) {
            __run_test(name, fn, true);
        };

        test.skip = function(name, fn) {
            __run_test(name, fn, true);
        };

        it.only = function(name, fn) {
            __run_test(name, fn, false);
        };

        test.only = function(name, fn) {
            __run_test(name, fn, false);
        };

        var __pledge_suite_first_test = {};

        function __run_test(name, fn, skip) {
            if (skip) {
                __pledge_test_results.push({
                    name: name,
                    suite: __pledge_current_suite,
                    status: "skipped",
                    error: null,
                    duration: 0
                });
                return;
            }

            // Run beforeAll hooks once per suite (on first test in the suite)
            if (!__pledge_suite_first_test[__pledge_current_suite]) {
                __pledge_suite_first_test[__pledge_current_suite] = true;
                for (var i = 0; i < __pledge_before_all.length; i++) {
                    try { __pledge_before_all[i](); } catch(e) {}
                }
            }

            // Run beforeEach hooks
            for (var i = 0; i < __pledge_before_each.length; i++) {
                try { __pledge_before_each[i](); } catch(e) {}
            }

            var start = Date.now();
            var error = null;
            try {
                fn();
            } catch(e) {
                error = e.message || String(e);
            }
            var duration = Date.now() - start;

            // Run afterEach hooks
            for (var i = 0; i < __pledge_after_each.length; i++) {
                try { __pledge_after_each[i](); } catch(e) {}
            }

            __pledge_test_results.push({
                name: name,
                suite: __pledge_current_suite,
                status: error ? "failed" : "passed",
                error: error,
                duration: duration
            });
        }

        function beforeAll(fn) { __pledge_before_all.push(fn); }
        function beforeEach(fn) { __pledge_before_each.push(fn); }
        function afterEach(fn) { __pledge_after_each.push(fn); }
        function afterAll(fn) { __pledge_after_all.push(fn); }

        // expect() assertion library
        function expect(actual) {
            return {
                __actual: actual,
                toBe: function(expected) {
                    if (actual !== expected) {
                        throw new Error("Expected " + JSON.stringify(actual) + " to be " + JSON.stringify(expected));
                    }
                },
                toEqual: function(expected) {
                    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
                        throw new Error("Expected " + JSON.stringify(actual) + " to equal " + JSON.stringify(expected));
                    }
                },
                toBeTruthy: function() {
                    if (!actual) {
                        throw new Error("Expected " + JSON.stringify(actual) + " to be truthy");
                    }
                },
                toBeFalsy: function() {
                    if (actual) {
                        throw new Error("Expected " + JSON.stringify(actual) + " to be falsy");
                    }
                },
                toBeNull: function() {
                    if (actual !== null) {
                        throw new Error("Expected " + JSON.stringify(actual) + " to be null");
                    }
                },
                toBeUndefined: function() {
                    if (actual !== undefined) {
                        throw new Error("Expected " + JSON.stringify(actual) + " to be undefined");
                    }
                },
                toBeDefined: function() {
                    if (actual === undefined) {
                        throw new Error("Expected value to be defined");
                    }
                },
                toContain: function(item) {
                    if (typeof actual === 'string') {
                        if (actual.indexOf(item) === -1) {
                            throw new Error("Expected \"" + actual + "\" to contain \"" + item + "\"");
                        }
                    } else if (Array.isArray(actual)) {
                        if (actual.indexOf(item) === -1) {
                            throw new Error("Expected array to contain " + JSON.stringify(item));
                        }
                    } else {
                        throw new Error("toContain requires string or array");
                    }
                },
                toHaveLength: function(len) {
                    if (!actual || actual.length !== len) {
                        throw new Error("Expected length " + len + " but got " + (actual ? actual.length : "undefined"));
                    }
                },
                toThrow: function() {
                    if (typeof actual !== 'function') {
                        throw new Error("toThrow expects a function");
                    }
                    try {
                        actual();
                        throw new Error("Expected function to throw");
                    } catch(e) {
                        // Expected - function threw
                    }
                },
                not: {
                    toBe: function(expected) {
                        if (actual === expected) {
                            throw new Error("Expected " + JSON.stringify(actual) + " NOT to be " + JSON.stringify(expected));
                        }
                    },
                    toEqual: function(expected) {
                        if (JSON.stringify(actual) === JSON.stringify(expected)) {
                            throw new Error("Expected " + JSON.stringify(actual) + " NOT to equal " + JSON.stringify(expected));
                        }
                    },
                    toBeTruthy: function() {
                        if (actual) {
                            throw new Error("Expected " + JSON.stringify(actual) + " NOT to be truthy");
                        }
                    },
                    toBeFalsy: function() {
                        if (!actual) {
                            throw new Error("Expected " + JSON.stringify(actual) + " NOT to be falsy");
                        }
                    },
                    toBeNull: function() {
                        if (actual === null) {
                            throw new Error("Expected NOT to be null");
                        }
                    },
                    toContain: function(item) {
                        if (typeof actual === 'string' && actual.indexOf(item) !== -1) {
                            throw new Error("Expected \"" + actual + "\" NOT to contain \"" + item + "\"");
                        }
                        if (Array.isArray(actual) && actual.indexOf(item) !== -1) {
                            throw new Error("Expected array NOT to contain " + JSON.stringify(item));
                        }
                    },
                    toThrow: function() {
                        if (typeof actual !== 'function') return;
                        try {
                            actual();
                        } catch(e) {
                            throw new Error("Expected function NOT to throw but it threw: " + e.message);
                        }
                    }
                }
            };
        }

        // vi mock object (Vitest compatibility)
        var vi = {
            fn: function(impl) {
                var mockFn = impl || function(){};
                mockFn.mock = { calls: [], results: [] };
                return mockFn;
            },
            mock: function() {},
            unmock: function() {},
            spyOn: function(obj, method) { return obj[method]; },
            stubGlobal: function(name, value) { globalThis[name] = value; }
        };
    "#;

    let _ = ctx.eval::<(), _>(harness_code);
}

/// Set up console.log support
fn setup_console(ctx: &Ctx) {
    let globals = ctx.globals();
    let console = Object::new(ctx.clone()).unwrap();
    console
        .set(
            "log",
            Func::new(|args: Rest<String>| {
                tracing::info!("[test console] {}", args.0.join(" "));
            }),
        )
        .unwrap();
    globals.set("console", console).unwrap();
}

/// Set up a minimal module shim so import/export statements don't crash
fn setup_module_shim(ctx: &Ctx, _file_path: &Path) {
    // Provide a minimal `require` function that returns undefined
    let _ = ctx.globals().set(
        "require",
        Func::new(|_arg: Opt<String>| {
            // No-op: require() returns undefined in the test environment
        }),
    );
}

/// Strip TypeScript-specific syntax for QuickJS compatibility
/// Removes: type annotations, interfaces, enums, export/import type, as assertions
/// Strip TypeScript types and lower JSX for a test file, using the same
/// Oxc-based transform the production build pipeline uses — NOT the naive
/// character-level `strip_typescript` this replaces at both real call sites
/// (`run_test_file_with_config`'s main source and its setup files). That
/// function only ever rewrote ESM import/export lines; it never actually
/// erased TypeScript syntax (`: number`, interfaces, generics, `as` casts,
/// ...), so any real-world `.test.ts`/`.test.tsx` file with so much as one
/// type annotation fed literal TypeScript into QuickJS (a plain JS engine)
/// and failed with an opaque "Exception generated by QuickJS" — actually
/// just a JS parse error dressed up unhelpfully. `pledgepack_core::transform`
/// does real syntax lowering via Oxc; it intentionally leaves `import`/
/// `export` statements untouched (module resolution is the bundler's job,
/// not this single-file transform's), so `rewrite_esm_for_eval` below still
/// handles that part exactly as before.
fn strip_typescript(
    source: &str,
    file_path: &Path,
    config: &pledgepack_core::PledgeConfig,
) -> Result<String> {
    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    let kind = pledgepack_core::ModuleKind::from_extension(&ext);
    let file_path_str = pledgepack_core::normalize_path(file_path);
    let output = pledgepack_core::transform::transform(
        source,
        kind,
        &file_path_str,
        /* is_production */ false,
        config,
    )?;
    Ok(rewrite_esm_for_eval(&output.code))
}

/// Rewrite ESM `import`/`export` syntax into statements QuickJS's plain
/// `eval` can execute directly: imports become no-ops (test files reach
/// their dependencies — `describe`/`it`/`expect`/etc. — via the injected
/// globals set up by `setup_test_harness`, not real module resolution),
/// and `export` declarations become plain variable/function/class
/// declarations (or, for `export default`, a `var __default =` assignment).
fn rewrite_esm_for_eval(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("import ") || trimmed.starts_with("export ") {
                // Keep export default as a variable assignment
                if trimmed.starts_with("export default") {
                    line.replace("export default", "var __default =")
                } else if trimmed.starts_with("export const")
                    || trimmed.starts_with("export let")
                    || trimmed.starts_with("export var")
                    || trimmed.starts_with("export function")
                    || trimmed.starts_with("export class")
                {
                    line.replace("export ", "")
                } else {
                    // Skip import statements
                    String::new()
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Set up test environment (jsdom/happy-dom shims or node defaults)
fn setup_test_environment(ctx: &Ctx, environment: &str) {
    let env_code = match environment {
        "jsdom" => {
            r#"
            // Minimal jsdom-like environment shim
            var __pledge_document = {
                createElement: function(tag) {
                    return {
                        tagName: tag.toUpperCase(),
                        style: {},
                        children: [],
                        appendChild: function(child) { this.children.push(child); },
                        removeChild: function(child) { this.children = this.children.filter(function(c) { return c !== child; }); },
                        addEventListener: function() {},
                        removeEventListener: function() {},
                        setAttribute: function() {},
                        getAttribute: function() { return null; },
                        classList: { add: function() {}, remove: function() {}, contains: function() { return false; }, toggle: function() {} },
                        innerHTML: '',
                        textContent: '',
                        onclick: null,
                    };
                },
                createTextNode: function(text) { return { textContent: text, nodeType: 3 }; },
                getElementById: function() { return null; },
                querySelector: function() { return null; },
                querySelectorAll: function() { return []; },
                body: { appendChild: function() {}, removeChild: function() {}, innerHTML: '' },
                head: { appendChild: function() {}, removeChild: function() {} },
                documentElement: { lang: 'en' },
                addEventListener: function() {},
                removeEventListener: function() {},
                readyState: 'complete',
                cookie: '',
            };
            globalThis.document = __pledge_document;
            globalThis.window = globalThis;
            globalThis.navigator = { userAgent: 'jsdom', platform: 'node' };
            globalThis.location = { href: 'http://localhost/', hostname: 'localhost', pathname: '/', search: '', hash: '' };
            globalThis.HTMLElement = function() {};
            globalThis.customElements = { define: function() {}, get: function() { return undefined; } };
            globalThis.MutationObserver = function() { this.observe = function() {}; this.disconnect = function() {}; };
            globalThis.getComputedStyle = function() { return { getPropertyValue: function() { return ''; } }; };
        "#
        }
        "happy-dom" => {
            r#"
            // Minimal happy-dom-like environment shim (similar to jsdom but lighter)
            var __pledge_document = {
                createElement: function(tag) {
                    return { tagName: tag.toUpperCase(), style: {}, children: [], appendChild: function(c) { this.children.push(c); }, setAttribute: function() {}, getAttribute: function() { return null; }, classList: { add: function() {}, remove: function() {}, contains: function() { return false; } }, innerHTML: '', textContent: '' };
                },
                createTextNode: function(t) { return { textContent: t }; },
                getElementById: function() { return null; },
                querySelector: function() { return null; },
                querySelectorAll: function() { return []; },
                body: { appendChild: function() {}, innerHTML: '' },
                head: { appendChild: function() {} },
                addEventListener: function() {},
                readyState: 'complete',
            };
            globalThis.document = __pledge_document;
            globalThis.window = globalThis;
            globalThis.navigator = { userAgent: 'happy-dom', platform: 'node' };
            globalThis.location = { href: 'http://localhost/', hostname: 'localhost' };
            globalThis.customElements = { define: function() {}, get: function() { return undefined; } };
            globalThis.MutationObserver = function() { this.observe = function() {}; this.disconnect = function() {}; };
        "#
        }
        _ => {
            r#"
            // Node.js environment — no DOM shims needed
            // Provide minimal process and Buffer stubs
            if (typeof process === 'undefined') {
                globalThis.process = { env: {}, argv: [], cwd: function() { return '.'; }, platform: 'node' };
            }
            if (typeof Buffer === 'undefined') {
                globalThis.Buffer = { from: function(s) { return s; }, concat: function(arrs) { return arrs.join(''); } };
            }
        "#
        }
    };
    let _ = ctx.eval::<(), _>(env_code);
}

/// Set up snapshot testing API (toMatchSnapshot, toMatchInlineSnapshot)
fn setup_snapshot_api(ctx: &Ctx, _store: &Arc<Mutex<SnapshotStore>>, update: bool) {
    // Inject __pledge_snapshot_data as a global object that expect() can access
    let snapshot_code = format!(
        r#"
        globalThis.__pledge_snapshot_store = {{ }};
        globalThis.__pledge_snapshot_update = {};
        globalThis.__pledge_snapshot_seq = 0;

        // toMatchSnapshot is handled inside expect() via a special marker
        globalThis.__pledge_match_snapshot = function(value, hint) {{
            var seq = ++globalThis.__pledge_snapshot_seq;
            var key = (hint || 'snapshot') + ' ' + seq;
            var serialized = JSON.stringify(value, null, 2);
            // Store for later comparison by Rust side
            globalThis.__pledge_snapshot_store[key] = serialized;
            return serialized;
        }};
        "#,
        if update { "true" } else { "false" }
    );
    let _ = ctx.eval::<(), _>(snapshot_code.as_str());

    // Add toMatchSnapshot and toMatchInlineSnapshot to the expect prototype
    let snapshot_extension = r#"
        var __original_expect = expect;
        expect = function(actual) {
            var result = __original_expect(actual);
            result.toMatchSnapshot = function(hint) {
                var seq = ++globalThis.__pledge_snapshot_seq;
                var key = (hint || 'snapshot') + ' ' + seq;
                var serialized = JSON.stringify(actual, null, 2);
                var stored = globalThis.__pledge_snapshot_store[key];
                if (globalThis.__pledge_snapshot_update || !stored) {
                    globalThis.__pledge_snapshot_store[key] = serialized;
                } else if (stored !== serialized) {
                    throw new Error("Snapshot mismatch for '" + key + "':\n  Expected: " + stored + "\n  Received: " + serialized);
                }
            };
            result.toMatchInlineSnapshot = function(snapshot) {
                if (snapshot === undefined) {
                    // Auto-generate inline snapshot
                    var serialized = JSON.stringify(actual, null, 2);
                    // Store for later extraction
                    if (!globalThis.__pledge_inline_snapshots) globalThis.__pledge_inline_snapshots = [];
                    globalThis.__pledge_inline_snapshots.push(serialized);
                } else {
                    var serialized = JSON.stringify(actual, null, 2);
                    if (snapshot !== serialized) {
                        throw new Error("Inline snapshot mismatch:\n  Expected: " + snapshot + "\n  Received: " + serialized);
                    }
                }
            };
            return result;
        };
    "#;
    let _ = ctx.eval::<(), _>(snapshot_extension);
}

type CoverageData = Arc<Mutex<Vec<(String, usize, usize)>>>;

/// Set up coverage tracking instrumentation
fn setup_coverage_tracking(ctx: &Ctx, _coverage_data: &CoverageData, file_path: &Path) {
    let file_str = pledgepack_core::normalize_path(file_path);
    let coverage_code = format!(
        r#"
        globalThis.__pledge_coverage = {{
            file: '{}',
            lines: {{}},
            functions: {{}},
            branches: {{}},
            recordLine: function(line) {{
                if (!this.lines[line]) this.lines[line] = 0;
                this.lines[line]++;
            }},
            recordFunction: function(name) {{
                if (!this.functions[name]) this.functions[name] = 0;
                this.functions[name]++;
            }},
            recordBranch: function(id) {{
                if (!this.branches[id]) this.branches[id] = 0;
                this.branches[id]++;
            }},
            getReport: function() {{
                var lineKeys = Object.keys(this.lines);
                var funcKeys = Object.keys(this.functions);
                var branchKeys = Object.keys(this.branches);
                return JSON.stringify({{
                    file: this.file,
                    linesTotal: lineKeys.length,
                    linesCovered: lineKeys.filter(function(k) {{ return this.lines[k] > 0; }}, this).length,
                    functionsTotal: funcKeys.length,
                    functionsCovered: funcKeys.filter(function(k) {{ return this.functions[k] > 0; }}, this).length,
                    branchesTotal: branchKeys.length,
                    branchesCovered: branchKeys.filter(function(k) {{ return this.branches[k] > 0; }}, this).length,
                }});
            }}
        }};
        "#,
        file_str
    );
    let _ = ctx.eval::<(), _>(coverage_code.as_str());
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    fn write_test(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn runaway_test_file_is_interrupted_by_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_test(&dir, "spin.test.js", "while (true) {}\n");
        let limits = SandboxLimits {
            memory_limit: 64 * 1024 * 1024,
            timeout: Duration::from_millis(300),
        };
        let start = Instant::now();
        let summary = run_test_file_with_limits(&file, &limits).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "hung past the timeout"
        );
        assert_eq!(summary.failed, 1, "{summary:?}");
        assert!(
            summary.results[0]
                .error
                .as_deref()
                .unwrap_or("")
                .contains("File evaluation error"),
            "{summary:?}"
        );
    }

    #[test]
    fn runaway_allocation_hits_the_memory_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_test(
            &dir,
            "alloc.test.js",
            "var a = []; for (var i = 0; i < 1e9; i++) { a.push('x'.repeat(1024) + i); }\n",
        );
        let limits = SandboxLimits {
            memory_limit: 16 * 1024 * 1024,
            timeout: Duration::from_secs(60),
        };
        let summary = run_test_file_with_limits(&file, &limits).unwrap();
        assert_eq!(summary.failed, 1, "{summary:?}");
    }

    #[test]
    fn normal_tests_still_pass_under_default_limits() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_test(
            &dir,
            "ok.test.js",
            "describe('s', function () { it('adds', function () { expect(1 + 1).toBe(2); }); });\n",
        );
        let summary = run_test_file(&file).unwrap();
        assert_eq!(summary.passed, 1, "{summary:?}");
        assert_eq!(summary.failed, 0, "{summary:?}");
    }
}
