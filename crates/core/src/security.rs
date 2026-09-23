// Security & Integrity features: #81 SRI hashes, #82 CSP generation,
// #83 dependency vulnerability scanning, #84 license compliance checking.

use base64::{Engine, engine::general_purpose};
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// Validates that a resolved path is within the given base directory.
/// Returns the canonicalized path if safe, or None if path traversal is detected.
fn safe_path_within(base: &Path, input: &str) -> Option<PathBuf> {
    // Skip external URLs
    if input.starts_with("http://") || input.starts_with("https://") || input.starts_with("//") {
        return None;
    }
    let joined = base.join(input.trim_start_matches('/'));
    // Lexically normalize to detect traversal
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut base_normalized = PathBuf::new();
    for component in base.components() {
        match component {
            std::path::Component::ParentDir => {
                base_normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => base_normalized.push(other.as_os_str()),
        }
    }
    if normalized.starts_with(&base_normalized) {
        Some(normalized)
    } else {
        None
    }
}

/// Generate a cryptographically random hex token — e.g. for the dev
/// server's access token (PRODUCTION-READINESS-100.md goal 18). `byte_len`
/// is the number of random bytes before hex-encoding, so the returned
/// string is `byte_len * 2` hex characters.
pub fn generate_random_token(byte_len: usize) -> String {
    let mut bytes = vec![0u8; byte_len];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    hex::encode(bytes)
}

// ── Feature 81: Subresource Integrity (SRI) hashes ────────────────────

/// Generate SRI hash for a file's content.
pub fn generate_sri_hash(content: &[u8]) -> String {
    let hash = simple_sha256(content);
    let b64 = base64_encode(&hash);
    format!("sha256-{}", b64)
}

/// Generate SRI integrity attributes for all script and link tags in HTML.
pub fn inject_sri_into_html(html: &str, out_dir: &Path) -> String {
    let mut result = html.to_string();
    let document = Html::parse_fragment(html);

    // Find all <script src="..."> tags. Selector parsing can only fail for a
    // malformed selector string — since this one is a static literal, a
    // failure here means the selector itself is broken, not that this HTML
    // document is unusual, so skip this pass and log rather than panic the
    // whole build over a CSS-selector-syntax detail unrelated to the user's
    // code. See PRODUCTION-READINESS-100.md goal 20.
    let Ok(script_sel) = Selector::parse("script[src]") else {
        warn!(
            "SRI: failed to parse internal selector 'script[src]', skipping script SRI injection"
        );
        return html.to_string();
    };
    for element in document.select(&script_sel) {
        let Some(src) = element.value().attr("src") else {
            continue;
        };
        // Skip external URLs — cannot compute SRI without local file access
        if src.starts_with("http://") || src.starts_with("https://") || src.starts_with("//") {
            continue;
        }
        match safe_path_within(out_dir, src) {
            Some(file_path) => {
                if file_path.is_file()
                    && let Ok(content) = std::fs::read(&file_path)
                {
                    let integrity = generate_sri_hash(&content);
                    let old = format!(r#"<script src="{}""#, src);
                    let new = format!(
                        r#"<script src="{}" integrity="{}" crossorigin="anonymous""#,
                        src, integrity
                    );
                    result = result.replace(&old, &new);
                }
            }
            None => {
                warn!(
                    "Skipping SRI for script src='{}': path traversal detected",
                    src
                );
            }
        }
    }

    // Find all <link rel="stylesheet" href="..."> tags
    let Ok(link_sel) = Selector::parse("link[rel='stylesheet'][href]") else {
        warn!(
            "SRI: failed to parse internal selector 'link[rel=stylesheet][href]', skipping stylesheet SRI injection"
        );
        return result;
    };
    for element in document.select(&link_sel) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        // Skip external URLs — cannot compute SRI without local file access
        if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("//") {
            continue;
        }
        match safe_path_within(out_dir, href) {
            Some(file_path) => {
                if file_path.is_file()
                    && let Ok(content) = std::fs::read(&file_path)
                {
                    let integrity = generate_sri_hash(&content);
                    let old = format!(r#"href="{}""#, href);
                    let new = format!(
                        r#"href="{}" integrity="{}" crossorigin="anonymous""#,
                        href, integrity
                    );
                    result = result.replace(&old, &new);
                }
            }
            None => {
                warn!(
                    "Skipping SRI for stylesheet href='{}': path traversal detected",
                    href
                );
            }
        }
    }

    result
}

// ── Feature 82: Content Security Policy generation ────────────────────

pub struct CspGenerator {
    script_src: Vec<String>,
    style_src: Vec<String>,
    img_src: Vec<String>,
    font_src: Vec<String>,
    connect_src: Vec<String>,
    inline_script_hashes: Vec<String>,
    inline_style_hashes: Vec<String>,
}

impl CspGenerator {
    pub fn new() -> Self {
        Self {
            script_src: vec!["'self'".to_string()],
            style_src: vec!["'self'".to_string()],
            img_src: vec!["'self'".to_string(), "data:".to_string()],
            font_src: vec!["'self'".to_string()],
            connect_src: vec!["'self'".to_string()],
            inline_script_hashes: Vec::new(),
            inline_style_hashes: Vec::new(),
        }
    }

    pub fn analyze_html(&mut self, html: &str) {
        let document = Html::parse_fragment(html);

        // Inline scripts: <script> tags without a src attribute. See the
        // comment on the selectors in `inject_sri_into_html` above — these
        // are static literals, so a parse failure means the selector itself
        // is broken; skip that pass rather than panic (goal 20).
        match Selector::parse("script") {
            Ok(script_sel) => {
                for element in document.select(&script_sel) {
                    // Skip scripts with src= attribute (external scripts)
                    if element.value().attr("src").is_some() {
                        continue;
                    }
                    let inline_code: String = element.text().collect::<String>();
                    let trimmed = inline_code.trim();
                    if !trimmed.is_empty() {
                        let hash = generate_sri_hash(trimmed.as_bytes());
                        self.inline_script_hashes.push(format!("'{}'", hash));
                    }
                }
            }
            Err(_) => warn!(
                "CSP: failed to parse internal selector 'script', skipping inline-script hashing"
            ),
        }

        // Inline styles: <style> tags
        let Ok(style_sel) = Selector::parse("style") else {
            warn!("CSP: failed to parse internal selector 'style', skipping inline-style hashing");
            return;
        };
        for element in document.select(&style_sel) {
            let inline_css: String = element.text().collect::<String>();
            let trimmed = inline_css.trim();
            if !trimmed.is_empty() {
                let hash = generate_sri_hash(trimmed.as_bytes());
                self.inline_style_hashes.push(format!("'{}'", hash));
            }
        }
    }

    pub fn add_script_src(&mut self, src: &str) {
        self.script_src.push(src.to_string());
    }

    pub fn add_style_src(&mut self, src: &str) {
        self.style_src.push(src.to_string());
    }

    pub fn generate(&self) -> String {
        let mut directives = Vec::new();
        directives.push("default-src 'self'".to_string());

        let mut scripts = self.script_src.clone();
        scripts.extend(self.inline_script_hashes.clone());
        directives.push(format!("script-src {}", scripts.join(" ")));

        let mut styles = self.style_src.clone();
        styles.extend(self.inline_style_hashes.clone());
        directives.push(format!("style-src {}", styles.join(" ")));

        directives.push(format!("img-src {}", self.img_src.join(" ")));
        directives.push(format!("font-src {}", self.font_src.join(" ")));
        directives.push(format!("connect-src {}", self.connect_src.join(" ")));
        directives.push("object-src 'none'".to_string());
        directives.push("base-uri 'self'".to_string());

        directives.join("; ")
    }

    pub fn generate_headers_file(&self, _out_dir: &Path) -> String {
        let csp = self.generate();
        format!(
            "/*\n  Content-Security-Policy: {}\n  X-Content-Type-Options: nosniff\n  X-Frame-Options: DENY\n  Referrer-Policy: strict-origin-when-cross-origin\n",
            csp
        )
    }
}

impl Default for CspGenerator {
    fn default() -> Self {
        Self::new()
    }
}

pub fn generate_csp_from_build(html: &str, out_dir: &Path) -> String {
    let mut csp_gen = CspGenerator::new();
    csp_gen.analyze_html(html);
    let headers = csp_gen.generate_headers_file(out_dir);

    let headers_path = out_dir.join("_headers");
    if let Err(e) = std::fs::write(&headers_path, &headers) {
        warn!("Failed to write _headers file: {}", e);
    } else {
        info!(
            "Generated CSP _headers file at {}",
            crate::display_path(&headers_path)
        );
    }

    csp_gen.generate()
}

// ── Feature 83: Dependency vulnerability scanning ─────────────────────

#[derive(Debug, Clone)]
pub struct Vulnerability {
    pub package: String,
    pub version: String,
    pub severity: VulnerabilitySeverity,
    pub title: String,
    pub cve: Option<String>,
    pub url: Option<String>,
    pub patch_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VulnerabilitySeverity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl VulnerabilitySeverity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Critical => "CRITICAL",
            Self::High => "HIGH",
            Self::Medium => "MEDIUM",
            Self::Low => "LOW",
            Self::Info => "INFO",
        }
    }
    pub fn color(&self) -> &'static str {
        match self {
            Self::Critical | Self::High => "\x1b[31m",
            Self::Medium => "\x1b[33m",
            Self::Low => "\x1b[36m",
            Self::Info => "\x1b[90m",
        }
    }
}

pub fn scan_vulnerabilities(root: &Path) -> Vec<Vulnerability> {
    let mut vulns = Vec::new();
    let pkg_json = root.join("package.json");
    if !pkg_json.is_file() {
        return vulns;
    }

    let content = match std::fs::read_to_string(&pkg_json) {
        Ok(c) => c,
        Err(_) => return vulns,
    };
    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(_) => return vulns,
    };

    let mut deps = HashMap::new();
    for key in &["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = json.get(key).and_then(|v| v.as_object()) {
            for (name, version) in obj {
                deps.insert(name.clone(), version.as_str().unwrap_or("*").to_string());
            }
        }
    }

    for (name, version) in &deps {
        if let Some(known) = check_advisory_database(name, version) {
            vulns.extend(known);
        }
    }

    if vulns.is_empty() {
        info!("No known vulnerabilities found in {} packages", deps.len());
    } else {
        warn!(
            "Found {} vulnerabilities in {} packages",
            vulns.len(),
            deps.len()
        );
    }
    vulns
}

fn check_advisory_database(package: &str, version: &str) -> Option<Vec<Vulnerability>> {
    let advisories: &[(&str, &str, &str, VulnerabilitySeverity, &str, &str, &str)] = &[
        (
            "lodash",
            "<4.17.21",
            "CVE-2021-23337",
            VulnerabilitySeverity::High,
            "Command injection via template",
            "https://npmjs.com/advisories/1673",
            "4.17.21",
        ),
        (
            "minimist",
            "<1.2.6",
            "CVE-2022-21222",
            VulnerabilitySeverity::Medium,
            "Prototype pollution",
            "https://npmjs.com/advisories/2392",
            "1.2.6",
        ),
        (
            "axios",
            "<0.21.1",
            "CVE-2021-3749",
            VulnerabilitySeverity::High,
            "SSRF vulnerability",
            "https://npmjs.com/advisories/1594",
            "0.21.1",
        ),
        (
            "ws",
            "<7.4.6",
            "CVE-2021-32615",
            VulnerabilitySeverity::Medium,
            "DoS via large WebSocket message",
            "https://npmjs.com/advisories/1748",
            "7.4.6",
        ),
        (
            "node-forge",
            "<1.3.0",
            "CVE-2022-24772",
            VulnerabilitySeverity::High,
            "Prototype pollution",
            "https://npmjs.com/advisories/2501",
            "1.3.0",
        ),
        (
            "minimatch",
            "<3.0.5",
            "CVE-2022-3517",
            VulnerabilitySeverity::High,
            "ReDoS via pattern",
            "https://npmjs.com/advisories/2513",
            "3.0.5",
        ),
        (
            "json-schema",
            "<0.4.0",
            "CVE-2021-27787",
            VulnerabilitySeverity::High,
            "Prototype pollution",
            "https://npmjs.com/advisories/1671",
            "0.4.0",
        ),
    ];

    let mut found = Vec::new();
    for (pkg, affected, cve, severity, title, url, patch) in advisories {
        if *pkg == package && version_matches(version, affected) {
            found.push(Vulnerability {
                package: package.to_string(),
                version: version.to_string(),
                severity: *severity,
                title: title.to_string(),
                cve: Some(cve.to_string()),
                url: Some(url.to_string()),
                patch_version: Some(patch.to_string()),
            });
        }
    }
    if found.is_empty() { None } else { Some(found) }
}

fn version_matches(version: &str, range: &str) -> bool {
    if range.starts_with('<') {
        let target = range.trim_start_matches('<').trim();
        return semver_less_than(version, target);
    }
    false
}

fn semver_less_than(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u32> {
        s.split('.')
            .filter_map(|p| p.split('-').next().and_then(|n| n.parse().ok()))
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    for i in 0..va.len().min(vb.len()) {
        if va[i] < vb[i] {
            return true;
        }
        if va[i] > vb[i] {
            return false;
        }
    }
    va.len() < vb.len()
}

pub fn format_vulnerability_report(vulns: &[Vulnerability]) -> String {
    if vulns.is_empty() {
        return "  \x1b[32m✓\x1b[0m No known vulnerabilities found".to_string();
    }
    let mut out = format!(
        "  \x1b[33m⚠\x1b[0m Found {} vulnerabilities\n\n",
        vulns.len()
    );
    for v in vulns {
        out.push_str(&format!(
            "  {}{}  {} {}\x1b[0m\n",
            v.severity.color(),
            v.severity.label(),
            v.package,
            v.version
        ));
        out.push_str(&format!("    {}\n", v.title));
        if let Some(ref cve) = v.cve {
            out.push_str(&format!("    CVE: {}\n", cve));
        }
        if let Some(ref url) = v.url {
            out.push_str(&format!("    URL: {}\n", url));
        }
        out.push('\n');
    }
    out
}

// ── Feature 84: License compliance checking ───────────────────────────

#[derive(Debug, Clone)]
pub struct LicenseInfo {
    pub package: String,
    pub version: String,
    pub license: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LicenseCheckResult {
    pub compliant: bool,
    pub licenses: Vec<LicenseInfo>,
    pub violations: Vec<LicenseViolation>,
}

#[derive(Debug, Clone)]
pub struct LicenseViolation {
    pub package: String,
    pub license: String,
    pub reason: String,
}

pub fn scan_licenses(root: &Path) -> Vec<LicenseInfo> {
    let mut licenses = Vec::new();
    let nm = root.join("node_modules");
    if !nm.is_dir() {
        return licenses;
    }
    scan_node_modules(&nm, &mut licenses);
    licenses
}

fn scan_node_modules(dir: &Path, licenses: &mut Vec<LicenseInfo>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('@') {
                if let Ok(sub_entries) = std::fs::read_dir(&path) {
                    for sub in sub_entries.flatten() {
                        let sub_path = sub.path();
                        if sub_path.is_dir()
                            && let Some(info) = read_package_license(&sub_path)
                        {
                            licenses.push(info);
                        }
                    }
                }
                continue;
            }
            if let Some(info) = read_package_license(&path) {
                licenses.push(info);
            }
        }
    }
}

fn read_package_license(pkg_dir: &Path) -> Option<LicenseInfo> {
    let pkg_json = pkg_dir.join("package.json");
    if !pkg_json.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&pkg_json).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    Some(LicenseInfo {
        package: json
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        version: json
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string(),
        license: json
            .get("license")
            .and_then(|v| {
                v.as_str().map(|s| s.to_string()).or_else(|| {
                    v.as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                })
            })
            .unwrap_or_else(|| "UNKNOWN".to_string()),
        path: pkg_dir.to_path_buf(),
    })
}

pub fn check_license_compliance(
    licenses: &[LicenseInfo],
    whitelist: &[&str],
    blacklist: &[&str],
) -> LicenseCheckResult {
    let mut violations = Vec::new();
    for info in licenses {
        let license_upper = info.license.to_uppercase();
        for bl in blacklist {
            if license_upper.contains(&bl.to_uppercase()) {
                violations.push(LicenseViolation {
                    package: info.package.clone(),
                    license: info.license.clone(),
                    reason: format!("License '{}' is blacklisted", info.license),
                });
                break;
            }
        }
        if !whitelist.is_empty() {
            if info.license == "UNKNOWN" {
                violations.push(LicenseViolation {
                    package: info.package.clone(),
                    license: info.license.clone(),
                    reason: "License is UNKNOWN — not in whitelist".to_string(),
                });
            } else if !whitelist
                .iter()
                .any(|w| license_upper.contains(&w.to_uppercase()))
            {
                violations.push(LicenseViolation {
                    package: info.package.clone(),
                    license: info.license.clone(),
                    reason: format!("License '{}' is not in whitelist", info.license),
                });
            }
        }
    }
    let compliant = violations.is_empty();
    if compliant {
        info!("License check: all {} packages compliant", licenses.len());
    } else {
        warn!(
            "License check: {} violations out of {} packages",
            violations.len(),
            licenses.len()
        );
    }
    LicenseCheckResult {
        compliant,
        licenses: licenses.to_vec(),
        violations,
    }
}

pub fn format_license_report(result: &LicenseCheckResult) -> String {
    let mut out = String::new();
    if result.compliant {
        out.push_str(&format!(
            "  \x1b[32m✓\x1b[0m All {} packages have compliant licenses\n",
            result.licenses.len()
        ));
    } else {
        out.push_str(&format!(
            "  \x1b[33m⚠\x1b[0m {} license violations found\n\n",
            result.violations.len()
        ));
        for v in &result.violations {
            out.push_str(&format!(
                "  \x1b[31m✗\x1b[0m {} — {}\n",
                v.package, v.reason
            ));
        }
    }
    let mut by_type: HashMap<String, usize> = HashMap::new();
    for info in &result.licenses {
        *by_type.entry(info.license.clone()).or_default() += 1;
    }
    out.push_str("\n  License summary:\n");
    let mut sorted: Vec<_> = by_type.into_iter().collect();
    sorted.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (license, count) in sorted {
        out.push_str(&format!(
            "    {} ({}): {} packages\n",
            license,
            if license == "UNKNOWN" {
                "\x1b[33m"
            } else {
                "\x1b[32m"
            },
            count
        ));
    }
    out
}

// ── Post-build secret scan ────────────────────────────────────────────
//
// The bundler is the last choke point before code reaches a browser. By the
// time a `ghp_…` token or a `.env` value is sitting inside an emitted chunk,
// every earlier safeguard has already been bypassed — so the emitted output
// itself gets scanned. Two confidence tiers:
//
//   * `Error` — recognized credential shapes (PEM blocks, AWS/GitHub/GCP/
//     Slack/Stripe tokens) and literal `.env` values that were not covered by
//     `env_prefix`. Effectively zero false positives; fails the build.
//   * `Warn` — high-entropy string literals that match no known shape.
//     Minified code and inline base64 can trip this, so it reports without
//     failing.

/// What was found in emitted output, with the secret itself redacted — a
/// scan report that echoes the credential would re-leak it into CI logs.
#[derive(Debug, Clone)]
pub struct SecretFinding {
    /// File path relative to the output directory.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// Credential shape, e.g. "GitHub token" or "env value `DB_PASSWORD`".
    pub kind: String,
    /// Where the secret appeared, with the middle elided (first 4 + last 2
    /// chars only — enough to identify which key leaked, never enough to
    /// use it).
    pub redacted: String,
    /// `true` = recognized credential / leaked env value → fail the build.
    /// `false` = entropy-only heuristic → warn.
    pub hard: bool,
}

fn redact(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 8 {
        return "***".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}…{tail}")
}

/// Credential shapes matched verbatim in emitted code. All are documented,
/// publicly-known token formats — a match is essentially never a false
/// positive.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    (
        "private key block",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----",
    ),
    ("AWS access key", r"\b(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}\b"),
    (
        "GitHub token",
        r"\b(?:ghp|gho|ghu|ghs|ghr|ghv)_[A-Za-z0-9]{36,}\b",
    ),
    (
        "GitHub fine-grained PAT",
        r"\bgithub_pat_[A-Za-z0-9_]{22,}\b",
    ),
    ("GCP API key", r"\bAIza[0-9A-Za-z_\-]{35}\b"),
    ("Slack token", r"\bxox[baprs]-[0-9A-Za-z\-]{10,}\b"),
    ("Stripe live key", r"\b[sr]k_live_[0-9A-Za-z]{16,}\b"),
    ("npm token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    (
        "SendGrid key",
        r"\bSG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}\b",
    ),
    (
        "generic bearer assignment",
        r#"(?i)(?:api[_-]?key|api[_-]?secret|access[_-]?token|auth[_-]?token|client[_-]?secret)["']?\s*[:=]\s*["'][A-Za-z0-9_\-/.+=]{24,}["']"#,
    ),
];

/// Minimum length for a `.env` value to be worth leak-checking — shorter
/// values ("true", "8080", "dev") would flag every chunk.
const MIN_ENV_VALUE_LEN: usize = 12;

/// Shannon entropy of a byte string in bits/char.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// Scan one emitted file's contents for embedded secrets.
///
/// `leaked_env` is `(var_name, value)` pairs for `.env` variables that do
/// NOT satisfy `env_prefix` — they must never appear in client output.
/// Values are never stored or returned, only their names and a redaction.
pub fn scan_code_for_secrets(
    code: &str,
    file: &str,
    leaked_env: &[(String, String)],
) -> Vec<SecretFinding> {
    let mut findings = Vec::new();

    for (kind, pattern) in SECRET_PATTERNS {
        let re = regex::Regex::new(pattern).expect("static secret pattern");
        for m in re.find_iter(code) {
            let line = code[..m.start()].bytes().filter(|&b| b == b'\n').count() + 1;
            findings.push(SecretFinding {
                file: file.to_string(),
                line,
                kind: kind.to_string(),
                redacted: redact(m.as_str()),
                hard: true,
            });
        }
    }

    for (name, value) in leaked_env {
        if value.len() >= MIN_ENV_VALUE_LEN && code.contains(value.as_str()) {
            // Find the first occurrence's line for the report.
            let pos = code.find(value.as_str()).unwrap_or(0);
            let line = code[..pos].bytes().filter(|&b| b == b'\n').count() + 1;
            findings.push(SecretFinding {
                file: file.to_string(),
                line,
                kind: format!("non-public env value `{name}` inlined"),
                redacted: redact(value),
                hard: true,
            });
        }
    }

    // Entropy tier: quoted literals that look random. Skip strings inside
    // sourcemap data URLs and integrity attributes — both are legitimately
    // high-entropy.
    let lit_re =
        regex::Regex::new(r#"["'`]([A-Za-z0-9+/=_\-]{24,})["'`]"#).expect("static literal pattern");
    for cap in lit_re.captures_iter(code) {
        let s = cap.get(1).unwrap().as_str();
        if s.starts_with("sha256-") || s.starts_with("data:") {
            continue;
        }
        if shannon_entropy(s) > 4.5 {
            let pos = cap.get(0).unwrap().start();
            let line = code[..pos].bytes().filter(|&b| b == b'\n').count() + 1;
            findings.push(SecretFinding {
                file: file.to_string(),
                line,
                kind: "high-entropy string (possible secret)".to_string(),
                redacted: redact(s),
                hard: false,
            });
        }
    }

    findings
}

/// Render findings for the console. Hard findings and warnings are split so
/// the caller can fail the build on the former only.
pub fn format_secret_findings(findings: &[SecretFinding]) -> String {
    let mut out = String::new();
    for f in findings {
        let marker = if f.hard { "✗" } else { "?" };
        out.push_str(&format!(
            "  {marker} {}:{} — {} ({})\n",
            f.file, f.line, f.kind, f.redacted
        ));
    }
    out
}

// ── Utility ───────────────────────────────────────────────────────────

fn simple_sha256(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn base64_encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sri_hash() {
        let hash = generate_sri_hash(b"test content");
        assert!(hash.starts_with("sha256-"));
    }

    #[test]
    fn test_csp_generation() {
        let mut csp_gen = CspGenerator::new();
        csp_gen.analyze_html(r#"<script>console.log("hello")</script>"#);
        let csp = csp_gen.generate();
        assert!(csp.contains("script-src"));
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("object-src 'none'"));
    }

    #[test]
    fn test_version_matches() {
        assert!(version_matches("1.2.3", "<1.3.0"));
        assert!(!version_matches("1.3.0", "<1.3.0"));
    }

    #[test]
    fn test_advisory_database() {
        assert!(check_advisory_database("lodash", "4.17.20").is_some());
        assert!(check_advisory_database("lodash", "4.17.21").is_none());
    }

    #[test]
    fn test_license_check() {
        let licenses = vec![
            LicenseInfo {
                package: "react".into(),
                version: "18.0.0".into(),
                license: "MIT".into(),
                path: PathBuf::from("nm/react"),
            },
            LicenseInfo {
                package: "gpl-pkg".into(),
                version: "1.0.0".into(),
                license: "GPL-3.0".into(),
                path: PathBuf::from("nm/gpl"),
            },
        ];
        let result = check_license_compliance(&licenses, &["MIT", "Apache-2.0"], &["GPL"]);
        assert!(!result.compliant);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hi"), "aGk=");
    }

    #[test]
    fn scan_detects_github_token() {
        let code = r#"const t = "ghp_abcdefghijklmnopqrstuvwxyz0123456789AB";"#;
        let findings = scan_code_for_secrets(code, "chunk.js", &[]);
        assert!(findings.iter().any(|f| f.hard && f.kind == "GitHub token"));
    }

    #[test]
    fn scan_detects_private_key_block() {
        let code = "const pem = \"-----BEGIN RSA PRIVATE KEY-----\\nMIIC...\"";
        let findings = scan_code_for_secrets(code, "chunk.js", &[]);
        assert!(
            findings
                .iter()
                .any(|f| f.hard && f.kind == "private key block")
        );
    }

    #[test]
    fn scan_detects_leaked_env_value() {
        let leaked = vec![(
            "DB_PASSWORD".to_string(),
            "sup3r-s3cret-p4ssw0rd".to_string(),
        )];
        let code = r#"const cfg = { pass: "sup3r-s3cret-p4ssw0rd" };"#;
        let findings = scan_code_for_secrets(code, "chunk.js", &leaked);
        assert!(
            findings
                .iter()
                .any(|f| f.hard && f.kind.contains("DB_PASSWORD")),
            "expected env-leak finding, got {findings:?}"
        );
        // The value itself must never appear in the report — only a redaction.
        assert!(!format_secret_findings(&findings).contains("sup3r-s3cret"));
    }

    #[test]
    fn scan_ignores_clean_code_and_short_env_values() {
        let code = "export function add(a, b) { return a + b; }";
        let leaked = vec![
            ("MODE".to_string(), "production".to_string()), // 10 chars — below min
            ("PORT".to_string(), "3000".to_string()),
        ];
        let findings = scan_code_for_secrets(code, "chunk.js", &leaked);
        assert!(
            findings.iter().all(|f| !f.hard),
            "clean code produced hard findings: {findings:?}"
        );
    }

    #[test]
    fn scan_redaction_never_reveals_full_secret() {
        let code = "const k = \"AKIAIOSFODNN7EXAMPLE\";";
        let findings = scan_code_for_secrets(code, "c.js", &[]);
        let report = format_secret_findings(&findings);
        assert!(!report.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(report.contains("AKIA"));
    }
}
