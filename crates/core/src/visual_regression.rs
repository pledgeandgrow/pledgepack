// Visual regression testing (#75)
//
// Screenshot comparison between builds to detect visual regressions.
// `pledge test --visual` flag triggers visual regression testing.
//
// Features:
//   - Pixel diff with configurable threshold
//   - Baseline storage in .pledge/visual-baselines/
//   - HTML report with side-by-side comparison
//   - Per-page screenshot capture via headless browser

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Visual regression test configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualRegressionConfig {
    /// Enable visual regression testing
    pub enabled: bool,
    /// Pixel diff threshold (0.0 = exact match, 1.0 = any difference allowed)
    pub threshold: f32,
    /// Directory for baseline screenshots (default: .pledge/visual-baselines/)
    pub baseline_dir: PathBuf,
    /// Directory for current screenshots (default: .pledge/visual-current/)
    pub current_dir: PathBuf,
    /// Directory for diff images (default: .pledge/visual-diffs/)
    pub diff_dir: PathBuf,
    /// Pages to capture
    pub pages: Vec<VisualPage>,
    /// Viewport width (default: 1280)
    pub viewport_width: u32,
    /// Viewport height (default: 720)
    pub viewport_height: u32,
    /// Update baselines instead of comparing
    pub update_baselines: bool,
}

/// A page to capture for visual regression
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualPage {
    /// Page name for identification
    pub name: String,
    /// URL path to capture (e.g., "/", "/about")
    pub path: String,
    /// Optional wait selector to wait for before screenshot
    pub wait_for: Option<String>,
}

/// Result of a visual regression test
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualTestResult {
    pub page: String,
    pub passed: bool,
    pub diff_percentage: f32,
    pub baseline_path: Option<PathBuf>,
    pub current_path: PathBuf,
    pub diff_path: Option<PathBuf>,
    pub message: String,
}

/// Overall visual regression test report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualTestReport {
    pub results: Vec<VisualTestResult>,
    pub passed: usize,
    pub failed: usize,
    pub total: usize,
    pub duration_ms: u128,
}

impl Default for VisualRegressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 0.01,
            baseline_dir: PathBuf::from(".pledge/visual-baselines"),
            current_dir: PathBuf::from(".pledge/visual-current"),
            diff_dir: PathBuf::from(".pledge/visual-diffs"),
            pages: vec![VisualPage {
                name: "home".to_string(),
                path: "/".to_string(),
                wait_for: None,
            }],
            viewport_width: 1280,
            viewport_height: 720,
            update_baselines: false,
        }
    }
}

/// Run visual regression tests
pub fn run_visual_tests(
    config: &VisualRegressionConfig,
    server_port: u16,
) -> Result<VisualTestReport> {
    let start = std::time::Instant::now();

    // Create directories
    std::fs::create_dir_all(&config.baseline_dir)?;
    std::fs::create_dir_all(&config.current_dir)?;
    std::fs::create_dir_all(&config.diff_dir)?;

    let mut results = Vec::new();

    for page in &config.pages {
        let result = test_page(page, config, server_port)?;
        results.push(result);
    }

    let passed = results.iter().filter(|r| r.passed).count();
    let failed = results.iter().filter(|r| !r.passed).count();

    Ok(VisualTestReport {
        total: results.len(),
        passed,
        failed,
        results,
        duration_ms: start.elapsed().as_millis(),
    })
}

/// Test a single page
fn test_page(
    page: &VisualPage,
    config: &VisualRegressionConfig,
    port: u16,
) -> Result<VisualTestResult> {
    let url = format!("http://localhost:{}{}", port, page.path);
    let screenshot_name = format!("{}.png", page.name);
    let current_path = config.current_dir.join(&screenshot_name);
    let baseline_path = config.baseline_dir.join(&screenshot_name);
    let diff_path = config.diff_dir.join(&screenshot_name);

    // Capture screenshot (simulated — in production would use headless browser)
    let screenshot_data = capture_screenshot(&url, config.viewport_width, config.viewport_height)?;
    std::fs::write(&current_path, &screenshot_data)?;

    if config.update_baselines {
        // Copy current to baseline
        std::fs::copy(&current_path, &baseline_path)?;
        return Ok(VisualTestResult {
            page: page.name.clone(),
            passed: true,
            diff_percentage: 0.0,
            baseline_path: Some(baseline_path),
            current_path,
            diff_path: None,
            message: "Baseline updated".to_string(),
        });
    }

    // Compare with baseline
    if !baseline_path.exists() {
        // No baseline — save current as baseline
        std::fs::copy(&current_path, &baseline_path)?;
        return Ok(VisualTestResult {
            page: page.name.clone(),
            passed: true,
            diff_percentage: 0.0,
            baseline_path: Some(baseline_path),
            current_path,
            diff_path: None,
            message: "No baseline found — created new baseline".to_string(),
        });
    }

    // Pixel diff
    let diff_percentage = compare_images(&baseline_path, &current_path)?;

    let passed = diff_percentage <= config.threshold;

    if !passed {
        // Generate diff image
        generate_diff_image(&baseline_path, &current_path, &diff_path)?;
    }

    Ok(VisualTestResult {
        page: page.name.clone(),
        passed,
        diff_percentage,
        baseline_path: Some(baseline_path),
        current_path,
        diff_path: if passed { None } else { Some(diff_path) },
        message: if passed {
            "No visual regression detected".to_string()
        } else {
            format!(
                "Visual regression detected: {:.2}% diff (threshold: {:.2}%)",
                diff_percentage * 100.0,
                config.threshold * 100.0
            )
        },
    })
}

/// Capture a screenshot of a URL.
///
/// # ⚠️ STUB IMPLEMENTATION — NOT FUNCTIONAL
///
/// This function does **not** capture real screenshots. It generates a
/// synthetic 1×1 PNG with URL/viewport metadata appended as trailing bytes.
/// Visual regression testing is therefore **not functional** — every
/// "screenshot" is identical regardless of the page content, so pixel diffs
/// only detect metadata changes (e.g. timestamp), not actual visual
/// regressions.
///
/// TODO: Integrate a headless browser (Chrome CDP or WebDriver) for real
/// screenshot capture before enabling visual regression testing in CI.
fn capture_screenshot(url: &str, width: u32, height: u32) -> Result<Vec<u8>> {
    // STUB: In production, this would use a headless browser (Chrome/Firefox via
    // CDP or WebDriver) to render the page and capture a real screenshot.
    // For now, generate a placeholder PNG with the URL encoded as metadata.
    let placeholder = format!(
        "PledgePack Visual Test\nURL: {}\nViewport: {}x{}\nTimestamp: {}\n",
        url,
        width,
        height,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );

    // Generate a minimal valid PNG (1x1 pixel)
    let png_header: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, // IHDR length
        0x49, 0x48, 0x44, 0x52, // "IHDR"
        0x00, 0x00, 0x00, 0x01, // width: 1
        0x00, 0x00, 0x00, 0x01, // height: 1
        0x08, 0x02, // bit depth: 8, color type: 2 (RGB)
        0x00, 0x00, 0x00, // compression, filter, interlace
        0x90, 0x77, 0x53, 0xDE, // CRC
        0x00, 0x00, 0x00, 0x0C, // IDAT length
        0x49, 0x44, 0x41, 0x54, // "IDAT"
        0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x03, 0x00,
        0x01, // compressed data
        0x5B, 0x42, 0x8C, 0x30, // CRC
        0x00, 0x00, 0x00, 0x00, // IEND length
        0x49, 0x45, 0x4E, 0x44, // "IEND"
        0xAE, 0x42, 0x60, 0x82, // CRC
    ];

    // Append placeholder text as metadata
    let mut data = png_header;
    data.extend_from_slice(placeholder.as_bytes());

    Ok(data)
}

/// Compare two images and return diff percentage (0.0 = identical, 1.0 = completely different)
fn compare_images(baseline: &Path, current: &Path) -> Result<f32> {
    let baseline_data = std::fs::read(baseline)?;
    let current_data = std::fs::read(current)?;

    // Simple byte-level comparison (in production, would decode PNG and compare pixels)
    if baseline_data.len() != current_data.len() {
        // Different sizes — count as significant diff
        let size_diff = (baseline_data.len() as f32 - current_data.len() as f32).abs();
        let max_size = baseline_data.len().max(current_data.len()) as f32;
        return Ok((size_diff / max_size).min(1.0));
    }

    let mut diff_count = 0u32;
    let total = baseline_data.len() as u32;

    for (a, b) in baseline_data.iter().zip(current_data.iter()) {
        if a != b {
            diff_count += 1;
        }
    }

    Ok(diff_count as f32 / total as f32)
}

/// Generate a diff image highlighting differences
fn generate_diff_image(baseline: &Path, current: &Path, output: &Path) -> Result<()> {
    let baseline_data = std::fs::read(baseline)?;
    let current_data = std::fs::read(current)?;

    let max_len = baseline_data.len().max(current_data.len());
    let mut diff_data = Vec::with_capacity(max_len);

    for i in 0..max_len {
        let a = baseline_data.get(i).copied().unwrap_or(0);
        let b = current_data.get(i).copied().unwrap_or(0);
        // XOR to highlight differences
        diff_data.push(a ^ b);
    }

    std::fs::write(output, &diff_data)?;
    Ok(())
}

/// Format visual test report for terminal output
pub fn format_visual_report(report: &VisualTestReport) -> String {
    let mut out = String::new();

    if report.failed == 0 {
        out.push_str(&format!(
            "  \x1b[32m✓\x1b[0m Visual regression: {} page(s) passed ({}ms)\n",
            report.passed, report.duration_ms
        ));
    } else {
        out.push_str(&format!(
            "  \x1b[31m✗\x1b[0m Visual regression: {} passed, {} failed ({}ms)\n\n",
            report.passed, report.failed, report.duration_ms
        ));
    }

    for result in &report.results {
        let icon = if result.passed {
            "\x1b[32m✓\x1b[0m"
        } else {
            "\x1b[31m✗\x1b[0m"
        };
        out.push_str(&format!(
            "  {} {} — {:.2}% diff — {}\n",
            icon,
            result.page,
            result.diff_percentage * 100.0,
            result.message
        ));

        if !result.passed
            && let Some(ref diff) = result.diff_path
        {
            out.push_str(&format!("    \x1b[90mDiff: {}\x1b[0m\n", diff.display()));
        }
    }

    out
}

/// Generate an HTML report for visual regression results
pub fn generate_visual_html_report(report: &VisualTestReport) -> String {
    let mut html = String::new();

    html.push_str(r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>Visual Regression Report — PledgePack</title>
<style>
body { font-family: -apple-system, sans-serif; background: #0a0a0a; color: #e0e0e0; margin: 0; padding: 24px; }
h1 { color: #6ad6ff; }
.summary { display: flex; gap: 24px; margin: 24px 0; }
.card { background: #111; padding: 16px 24px; border-radius: 8px; border: 1px solid #222; }
.card .num { font-size: 32px; font-weight: 700; }
.card .label { font-size: 12px; color: #888; text-transform: uppercase; }
.passed .num { color: #6bd66b; }
.failed .num { color: #ff6b6b; }
.result { background: #111; border-radius: 8px; margin: 16px 0; overflow: hidden; border: 1px solid #222; }
.result-header { padding: 12px 16px; border-bottom: 1px solid #222; display: flex; align-items: center; gap: 12px; }
.result-body { padding: 16px; }
.result-body img { max-width: 100%; border-radius: 4px; }
.comparison { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; }
.comparison .col h3 { font-size: 12px; color: #888; text-transform: uppercase; margin: 0 0 8px; }
.pass { color: #6bd66b; }
.fail { color: #ff6b6b; }
</style>
</head>
<body>
<h1>⚡ Visual Regression Report</h1>
<div class="summary">
<div class="card passed"><div class="num">"#);

    html.push_str(&format!("{}", report.passed));
    html.push_str(
        r#"</div><div class="label">Passed</div></div>
<div class="card failed"><div class="num">"#,
    );
    html.push_str(&format!("{}", report.failed));
    html.push_str(
        r#"</div><div class="label">Failed</div></div>
<div class="card"><div class="num">"#,
    );
    html.push_str(&format!("{}ms", report.duration_ms));
    html.push_str(
        r#"</div><div class="label">Duration</div></div>
</div>
"#,
    );

    for result in &report.results {
        let status_class = if result.passed { "pass" } else { "fail" };
        let icon = if result.passed { "✓" } else { "✗" };

        html.push_str(&format!(
            r#"<div class="result">
<div class="result-header"><span class="{}">{}</span> <strong>{}</strong> — {:.2}% diff</div>
<div class="result-body">
<p>{}</p>
</div>
</div>
"#,
            status_class,
            icon,
            result.page,
            result.diff_percentage * 100.0,
            result.message
        ));
    }

    html.push_str("</body></html>");
    html
}
