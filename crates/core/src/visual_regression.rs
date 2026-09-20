// Visual regression testing (#75)
//
// Screenshot comparison between builds to detect visual regressions.
// `pledge test --visual` flag triggers visual regression testing.
//
// Features:
//   - Pixel diff with configurable threshold
//   - Baseline storage in .pledge/visual-baselines/
//   - HTML report with side-by-side comparison
//   - Per-page screenshot capture via headless Chrome/Chromium/Edge (PLEDGE_CHROME to override)

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

    // Capture a real screenshot with a headless Chromium-family browser
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

/// Locate a Chromium-family browser for headless screenshots.
///
/// Order: `PLEDGE_CHROME` env var (explicit override), then well-known
/// executable names on `PATH`, then well-known install locations.
fn find_chrome() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PLEDGE_CHROME") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
        "msedge",
        "microsoft-edge",
    ];
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in names {
                for candidate in [dir.join(name), dir.join(format!("{name}.exe"))] {
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    let known = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    known.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Capture a real PNG screenshot of `url` using a headless Chromium-family
/// browser (`--headless --screenshot`). Requires Chrome, Chromium or Edge:
/// it is located via `PLEDGE_CHROME`, `PATH`, or well-known install paths.
///
/// Returns an error — never a placeholder image — when no browser is found or
/// the browser fails to produce a screenshot, so visual regression can never
/// silently "pass" without having compared real pixels.
fn capture_screenshot(url: &str, width: u32, height: u32) -> Result<Vec<u8>> {
    let chrome = find_chrome().ok_or_else(|| {
        anyhow::anyhow!(
            "visual regression needs Chrome, Chromium or Edge for headless screenshots, but none \
             was found (install one or set PLEDGE_CHROME to its executable path)"
        )
    })?;

    let workdir = std::env::temp_dir().join(format!(
        "pledge-shot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&workdir)
        .map_err(|e| anyhow::anyhow!("failed to create screenshot temp dir: {e}"))?;
    let out = workdir.join("shot.png");
    let profile = workdir.join("profile");

    let output = std::process::Command::new(&chrome)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-sandbox")
        .arg("--hide-scrollbars")
        .arg("--force-device-scale-factor=1")
        .arg("--virtual-time-budget=5000")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--window-size={width},{height}"))
        .arg(format!("--screenshot={}", out.display()))
        .arg(url)
        .output()
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&workdir);
            anyhow::anyhow!("failed to launch {}: {e}", crate::display_path(&chrome))
        })?;

    let shot = std::fs::read(&out);
    let _ = std::fs::remove_dir_all(&workdir);
    match shot {
        Ok(bytes) if !bytes.is_empty() => Ok(bytes),
        _ => anyhow::bail!(
            "{} did not produce a screenshot for {url} (exit: {}): {}",
            crate::display_path(&chrome),
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap_or("")
        ),
    }
}

/// Compare two PNG images pixel by pixel and return the fraction of differing
/// pixels (0.0 = identical, 1.0 = completely different). Images with different
/// dimensions are treated as completely different.
fn compare_images(baseline: &Path, current: &Path) -> Result<f32> {
    let a = image::open(baseline)
        .map_err(|e| anyhow::anyhow!("cannot decode baseline {}: {e}", crate::display_path(&baseline)))?
        .to_rgba8();
    let b = image::open(current)
        .map_err(|e| anyhow::anyhow!("cannot decode screenshot {}: {e}", crate::display_path(&current)))?
        .to_rgba8();

    if a.dimensions() != b.dimensions() {
        return Ok(1.0);
    }
    let total = (a.width() as u64) * (a.height() as u64);
    if total == 0 {
        return Ok(0.0);
    }
    let differing = a.pixels().zip(b.pixels()).filter(|(p, q)| p != q).count() as u64;
    Ok(differing as f32 / total as f32)
}

/// Generate a diff image: unchanged pixels are dimmed, changed pixels red.
fn generate_diff_image(baseline: &Path, current: &Path, output: &Path) -> Result<()> {
    let a = image::open(baseline)
        .map_err(|e| anyhow::anyhow!("cannot decode baseline {}: {e}", crate::display_path(&baseline)))?
        .to_rgba8();
    let b = image::open(current)
        .map_err(|e| anyhow::anyhow!("cannot decode screenshot {}: {e}", crate::display_path(&current)))?
        .to_rgba8();

    let width = a.width().max(b.width());
    let height = a.height().max(b.height());
    let mut diff = image::RgbaImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let pa = (x < a.width() && y < a.height()).then(|| *a.get_pixel(x, y));
            let pb = (x < b.width() && y < b.height()).then(|| *b.get_pixel(x, y));
            let px = match (pa, pb) {
                (Some(p), Some(q)) if p == q => image::Rgba([p[0] / 3, p[1] / 3, p[2] / 3, 255]),
                _ => image::Rgba([255, 0, 0, 255]),
            };
            diff.put_pixel(x, y, px);
        }
    }
    diff.save_with_format(output, image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("cannot write diff image {}: {e}", crate::display_path(&output)))?;
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
            out.push_str(&format!("    \x1b[90mDiff: {}\x1b[0m\n", crate::display_path(&diff)));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, w: u32, h: u32, fill: [u8; 4], dot: Option<(u32, u32)>) {
        let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba(fill));
        if let Some((x, y)) = dot {
            img.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
        }
        img.save_with_format(path, image::ImageFormat::Png).unwrap();
    }

    /// Only runs where a Chromium-family browser exists (developer machines,
    /// CI images that ship Chrome); elsewhere it is a no-op so the suite stays
    /// hermetic. Verifies the screenshot is a real PNG of the requested size.
    #[test]
    fn headless_capture_produces_real_png_when_browser_available() {
        if find_chrome().is_none() {
            eprintln!("skipping: no Chromium-family browser found");
            return;
        }
        let bytes = capture_screenshot(
            "data:text/html,<body style='background:red'><h1>hi</h1></body>",
            320,
            200,
        )
        .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        assert!(img.width() > 0 && img.height() > 0);
    }

    #[test]
    fn identical_images_have_zero_diff() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.png"), d.path().join("b.png"));
        write_png(&a, 10, 10, [10, 20, 30, 255], None);
        write_png(&b, 10, 10, [10, 20, 30, 255], None);
        assert_eq!(compare_images(&a, &b).unwrap(), 0.0);
    }

    #[test]
    fn one_changed_pixel_is_one_percent_of_a_10x10_image() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.png"), d.path().join("b.png"));
        write_png(&a, 10, 10, [10, 20, 30, 255], None);
        write_png(&b, 10, 10, [10, 20, 30, 255], Some((3, 4)));
        let diff = compare_images(&a, &b).unwrap();
        assert!((diff - 0.01).abs() < 1e-6, "{diff}");
    }

    #[test]
    fn different_dimensions_are_fully_different() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.png"), d.path().join("b.png"));
        write_png(&a, 10, 10, [0, 0, 0, 255], None);
        write_png(&b, 12, 10, [0, 0, 0, 255], None);
        assert_eq!(compare_images(&a, &b).unwrap(), 1.0);
    }

    #[test]
    fn non_png_input_is_an_error_not_a_pass() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a.png"), d.path().join("b.png"));
        write_png(&a, 4, 4, [0, 0, 0, 255], None);
        std::fs::write(&b, b"not a png").unwrap();
        assert!(compare_images(&a, &b).is_err());
    }

    #[test]
    fn diff_image_marks_changed_pixel_red() {
        let d = tempfile::tempdir().unwrap();
        let (a, b, out) = (
            d.path().join("a.png"),
            d.path().join("b.png"),
            d.path().join("diff.png"),
        );
        write_png(&a, 5, 5, [90, 90, 90, 255], None);
        write_png(&b, 5, 5, [90, 90, 90, 255], Some((2, 2)));
        generate_diff_image(&a, &b, &out).unwrap();
        let diff = image::open(&out).unwrap().to_rgba8();
        assert_eq!(diff.get_pixel(2, 2).0, [255, 0, 0, 255]);
        assert_eq!(diff.get_pixel(0, 0).0, [30, 30, 30, 255]);
    }
}
