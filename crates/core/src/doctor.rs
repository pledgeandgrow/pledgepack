// Doctor — diagnostics for build issues, config problems, missing deps, and perf bottlenecks.

use std::path::Path;

#[derive(Debug, Clone)]
pub struct DiagnosticReport {
    pub checks: Vec<DiagnosticCheck>,
    pub summary: DiagnosticSummary,
}

#[derive(Debug, Clone)]
pub struct DiagnosticCheck {
    pub category: DiagnosticCategory,
    pub status: DiagnosticStatus,
    pub name: String,
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticCategory {
    Config,
    Dependencies,
    Performance,
    Project,
    Security,
}

impl DiagnosticCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Config => "Config",
            Self::Dependencies => "Dependencies",
            Self::Performance => "Performance",
            Self::Project => "Project",
            Self::Security => "Security",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Pass,
    Warn,
    Fail,
    Info,
}

impl DiagnosticStatus {
    pub fn icon(&self) -> &'static str {
        match self {
            Self::Pass => "\x1b[32m✓\x1b[0m",
            Self::Warn => "\x1b[33m⚠\x1b[0m",
            Self::Fail => "\x1b[31m✗\x1b[0m",
            Self::Info => "\x1b[36mℹ\x1b[0m",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiagnosticSummary {
    pub passed: usize,
    pub warnings: usize,
    pub failed: usize,
    pub info: usize,
}

/// Run all diagnostic checks on a project.
pub fn run_diagnostics(root: &Path, config: &crate::config::PledgeConfig) -> DiagnosticReport {
    let mut checks = Vec::new();

    // Config checks
    checks.extend(check_config(root, config));

    // Dependency checks
    checks.extend(check_dependencies(root));

    // Performance checks
    checks.extend(check_performance(root, config));

    // Project structure checks
    checks.extend(check_project_structure(root, config));

    // Security checks
    checks.extend(check_security(root));

    // Toolchain checks (Windows linker shadowing, etc.)
    checks.extend(check_toolchain());

    let passed = checks
        .iter()
        .filter(|c| c.status == DiagnosticStatus::Pass)
        .count();
    let warnings = checks
        .iter()
        .filter(|c| c.status == DiagnosticStatus::Warn)
        .count();
    let failed = checks
        .iter()
        .filter(|c| c.status == DiagnosticStatus::Fail)
        .count();
    let info = checks
        .iter()
        .filter(|c| c.status == DiagnosticStatus::Info)
        .count();

    DiagnosticReport {
        checks,
        summary: DiagnosticSummary {
            passed,
            warnings,
            failed,
            info,
        },
    }
}

fn check_config(root: &Path, config: &crate::config::PledgeConfig) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check config file exists
    let has_ts_config = root.join("pledge.config.ts").exists()
        || root.join("pledge.config.js").exists()
        || root.join("pledge.config.mjs").exists();
    let has_json_config =
        root.join("pledge.json").exists() || root.join("pledge.config.json").exists();

    if has_ts_config {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Pass,
            name: "Config file".to_string(),
            message: "pledge.config.ts found".to_string(),
            suggestion: None,
        });
    } else if has_json_config {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Info,
            name: "Config file".to_string(),
            message: "JSON config found — consider using pledge.config.ts for TypeScript support"
                .to_string(),
            suggestion: Some(
                "Rename to pledge.config.ts for autocompletion and type checking".to_string(),
            ),
        });
    } else {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Warn,
            name: "Config file".to_string(),
            message: "No config file found — using defaults".to_string(),
            suggestion: Some("Run `pledgepack init` to generate a config file".to_string()),
        });
    }

    // PledgeStack apps (`framework: 'pledge'`) have no SPA `entry`/`index.html`
    // — routes under app/ are the entry surface; skip both checks for them.
    let is_pledgestack = config.framework == crate::config::Framework::Pledge;

    // Check entry file exists
    if !is_pledgestack {
        for entry in &config.entry {
            let entry_path = root.join(entry);
            if entry_path.exists() {
                checks.push(DiagnosticCheck {
                    category: DiagnosticCategory::Config,
                    status: DiagnosticStatus::Pass,
                    name: "Entry file".to_string(),
                    message: format!("{} exists", entry),
                    suggestion: None,
                });
            } else {
                checks.push(DiagnosticCheck {
                    category: DiagnosticCategory::Config,
                    status: DiagnosticStatus::Fail,
                    name: "Entry file".to_string(),
                    message: format!("{} not found", entry),
                    suggestion: Some(format!(
                        "Create {} or update `entry` in pledge.config.ts",
                        entry
                    )),
                });
            }
        }
    }

    // Check HTML entry
    if !is_pledgestack {
        let html_path = config.html_entry.as_deref().unwrap_or("index.html");
        if root.join(html_path).exists() {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Config,
                status: DiagnosticStatus::Pass,
                name: "HTML entry".to_string(),
                message: format!("{} found", html_path),
                suggestion: None,
            });
        } else {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Config,
                status: DiagnosticStatus::Warn,
                name: "HTML entry".to_string(),
                message: "index.html not found — Pledgepack will generate a default one"
                    .to_string(),
                suggestion: None,
            });
        }
    }

    // Check for conflicting build tools
    let conflicting = [
        "vite.config.ts",
        "vite.config.js",
        "webpack.config.js",
        "next.config.js",
        "next.config.ts",
    ];
    let found: Vec<&str> = conflicting
        .iter()
        .filter(|c| root.join(c).exists())
        .copied()
        .collect();
    if !found.is_empty() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Warn,
            name: "Conflicting configs".to_string(),
            message: format!("Found: {} — may conflict with Pledgepack", found.join(", ")),
            suggestion: Some(
                "Remove or rename conflicting config files after migration".to_string(),
            ),
        });
    }

    // Validate config fields
    checks.extend(validate_config_fields(config));

    checks
}

fn validate_config_fields(config: &crate::config::PledgeConfig) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();
    let valid_fields = [
        "entry",
        "outDir",
        "root",
        "mode",
        "framework",
        "alias",
        "extensions",
        "cache",
        "devServer",
        "sourceMaps",
        "resolveAlias",
        "proxy",
        "profile",
        "outputFormat",
        "conditions",
        "envPrefix",
        "envDts",
        "htmlEntry",
        "compressGzip",
        "compressBrotli",
        "image",
        "edgeTarget",
        "plugins",
    ];

    // Check for known misconfigurations
    // (`framework: 'pledge'` apps route via app/ and have no `entry`.)
    if config.entry.is_empty() && config.framework != crate::config::Framework::Pledge {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Fail,
            name: "Entry points".to_string(),
            message: "No entry points configured".to_string(),
            suggestion: Some("Add at least one entry file in pledge.config.ts".to_string()),
        });
    }

    if config.dev_server.port < 1024 {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Warn,
            name: "Dev server port".to_string(),
            message: format!(
                "Port {} may require elevated privileges",
                config.dev_server.port
            ),
            suggestion: Some("Use a port above 1024 (e.g., 3000)".to_string()),
        });
    }

    if config.env_prefix.is_empty() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Config,
            status: DiagnosticStatus::Warn,
            name: "Env prefix".to_string(),
            message: "No env prefix set — all env vars will be exposed".to_string(),
            suggestion: Some("Set envPrefix to ['PLEDGE_'] to limit exposed variables".to_string()),
        });
    }

    let _ = valid_fields; // Used for future "did you mean" checking
    checks
}

fn check_dependencies(root: &Path) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    let pkg_path = root.join("package.json");
    if !pkg_path.exists() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Warn,
            name: "package.json".to_string(),
            message: "No package.json found".to_string(),
            suggestion: Some("Run `npm init` to create one".to_string()),
        });
        return checks;
    }

    let pkg_content = match std::fs::read_to_string(&pkg_path) {
        Ok(c) => c,
        Err(_) => {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Dependencies,
                status: DiagnosticStatus::Fail,
                name: "package.json".to_string(),
                message: "Cannot read package.json".to_string(),
                suggestion: None,
            });
            return checks;
        }
    };

    let pkg: serde_json::Value = match serde_json::from_str(&pkg_content) {
        Ok(v) => v,
        Err(e) => {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Dependencies,
                status: DiagnosticStatus::Fail,
                name: "package.json".to_string(),
                message: format!("Invalid JSON: {}", e),
                suggestion: Some("Fix JSON syntax errors in package.json".to_string()),
            });
            return checks;
        }
    };

    // Check for node_modules
    if root.join("node_modules").exists() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Pass,
            name: "node_modules".to_string(),
            message: "Dependencies installed".to_string(),
            suggestion: None,
        });
    } else {
        let pm = if root.join("pnpm-lock.yaml").exists() {
            "pnpm install"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else if root.join("bun.lockb").exists() {
            "bun install"
        } else {
            "npm install"
        };
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Fail,
            name: "node_modules".to_string(),
            message: "Dependencies not installed".to_string(),
            suggestion: Some(format!("Run `{}`", pm)),
        });
    }

    // Check for lock file
    let lock_files = [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
    ];
    let has_lock = lock_files.iter().any(|f| root.join(f).exists());
    if has_lock {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Pass,
            name: "Lock file".to_string(),
            message: "Lock file found".to_string(),
            suggestion: None,
        });
    } else {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Warn,
            name: "Lock file".to_string(),
            message: "No lock file found — reproducible installs not guaranteed".to_string(),
            suggestion: Some("Commit your lock file to version control".to_string()),
        });
    }

    // Check for outdated React/React DOM
    if let Some(deps) = pkg.get("dependencies")
        && let Some(react_ver) = deps.get("react").and_then(|v| v.as_str())
        && (react_ver.starts_with("16.") || react_ver.starts_with("17."))
    {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Info,
            name: "React version".to_string(),
            message: format!(
                "React {} — consider upgrading to 18+ for automatic JSX runtime",
                react_ver
            ),
            suggestion: Some("npm install react@18 react-dom@18".to_string()),
        });
    }

    // Check for duplicate deps (same package in deps and devDeps)
    let deps_keys: Vec<String> = pkg
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let dev_deps_keys: Vec<String> = pkg
        .get("devDependencies")
        .and_then(|d| d.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();

    let duplicates: Vec<&String> = deps_keys
        .iter()
        .filter(|k| dev_deps_keys.contains(k))
        .collect();
    if !duplicates.is_empty() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Dependencies,
            status: DiagnosticStatus::Warn,
            name: "Duplicate deps".to_string(),
            message: format!(
                "Packages in both deps and devDeps: {}",
                duplicates
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            suggestion: Some("Remove from devDependencies if already in dependencies".to_string()),
        });
    }

    checks
}

fn check_performance(root: &Path, config: &crate::config::PledgeConfig) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check cache status
    if config.cache.enabled {
        let cache_dir = root.join(&config.cache.dir);
        if cache_dir.exists() {
            let entries = std::fs::read_dir(&cache_dir)
                .map(|d| d.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Performance,
                status: DiagnosticStatus::Pass,
                name: "Cache".to_string(),
                message: format!("Cache enabled ({} entries)", entries),
                suggestion: None,
            });
        } else {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Performance,
                status: DiagnosticStatus::Info,
                name: "Cache".to_string(),
                message: "Cache enabled but empty — will populate on first build".to_string(),
                suggestion: None,
            });
        }
    } else {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Performance,
            status: DiagnosticStatus::Warn,
            name: "Cache".to_string(),
            message: "Cache disabled — builds will be slower".to_string(),
            suggestion: Some(
                "Enable cache in pledge.config.ts: cache: { enabled: true }".to_string(),
            ),
        });
    }

    // Check source maps in production
    if config.mode == crate::config::BuildMode::Production && config.source_maps {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Performance,
            status: DiagnosticStatus::Info,
            name: "Source maps".to_string(),
            message: "Source maps enabled in production — increases build size".to_string(),
            suggestion: Some(
                "Set sourceMaps: false for production or use hidden source maps".to_string(),
            ),
        });
    }

    // Check compression
    if !config.compress_gzip && !config.compress_brotli {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Performance,
            status: DiagnosticStatus::Info,
            name: "Compression".to_string(),
            message: "No compression enabled — consider gzip/brotli for production".to_string(),
            suggestion: Some(
                "Enable in config: compressGzip: true, compressBrotli: true".to_string(),
            ),
        });
    } else {
        let mut types = Vec::new();
        if config.compress_gzip {
            types.push("gzip");
        }
        if config.compress_brotli {
            types.push("brotli");
        }
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Performance,
            status: DiagnosticStatus::Pass,
            name: "Compression".to_string(),
            message: format!("Compression enabled: {}", types.join(" + ")),
            suggestion: None,
        });
    }

    // Check for large node_modules
    if root.join("node_modules").exists()
        && let Ok(entries) = std::fs::read_dir(root.join("node_modules"))
    {
        let count = entries.filter_map(|e| e.ok()).count();
        if count > 500 {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Performance,
                status: DiagnosticStatus::Warn,
                name: "Dependencies count".to_string(),
                message: format!("{} packages in node_modules — consider pruning", count),
                suggestion: Some("Run `npm prune` or audit for unused dependencies".to_string()),
            });
        }
    }

    checks
}

fn check_project_structure(
    root: &Path,
    config: &crate::config::PledgeConfig,
) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check for src directory (PledgeStack apps keep sources under app/ and
    // server/, so a missing src/ is expected, not a warning).
    let is_pledgestack = config.framework == crate::config::Framework::Pledge;
    if root.join("src").exists() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Pass,
            name: "src directory".to_string(),
            message: "src/ directory found".to_string(),
            suggestion: None,
        });
    } else if !is_pledgestack {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Warn,
            name: "src directory".to_string(),
            message: "No src/ directory — entry files may not be found".to_string(),
            suggestion: Some("Create a src/ directory for your source files".to_string()),
        });
    }

    // Check for .env files
    let has_env = root.join(".env").exists();
    let has_env_local = root.join(".env.local").exists();
    if has_env || has_env_local {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Pass,
            name: "Environment files".to_string(),
            message: if has_env_local {
                ".env and .env.local found".to_string()
            } else {
                ".env found".to_string()
            },
            suggestion: None,
        });
    } else {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Info,
            name: "Environment files".to_string(),
            message: "No .env files — environment variables will come from process.env only"
                .to_string(),
            suggestion: Some(
                "Create .env for default vars and .env.local for local overrides".to_string(),
            ),
        });
    }

    // Check for tsconfig.json
    if root.join("tsconfig.json").exists() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Pass,
            name: "TypeScript config".to_string(),
            message: "tsconfig.json found".to_string(),
            suggestion: None,
        });
    } else if config.framework != crate::config::Framework::Auto {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Warn,
            name: "TypeScript config".to_string(),
            message: "No tsconfig.json — TypeScript path aliases won't be resolved".to_string(),
            suggestion: Some(
                "Run `pledgepack create` to generate tsconfig.json or create one manually"
                    .to_string(),
            ),
        });
    }

    // Check .gitignore
    let gitignore_path = root.join(".gitignore");
    if gitignore_path.exists() {
        let gitignore = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
        let missing_entries: Vec<&str> =
            [".pledge", "node_modules", ".env.local", "pledge-env.d.ts"]
                .iter()
                .filter(|entry| !gitignore.contains(*entry))
                .copied()
                .collect();

        if missing_entries.is_empty() {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Project,
                status: DiagnosticStatus::Pass,
                name: ".gitignore".to_string(),
                message: "All Pledgepack entries present".to_string(),
                suggestion: None,
            });
        } else {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Project,
                status: DiagnosticStatus::Warn,
                name: ".gitignore".to_string(),
                message: format!("Missing entries: {}", missing_entries.join(", ")),
                suggestion: Some(format!(
                    "Add these to .gitignore: {}",
                    missing_entries.join("\n")
                )),
            });
        }
    } else {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Project,
            status: DiagnosticStatus::Warn,
            name: ".gitignore".to_string(),
            message: "No .gitignore found".to_string(),
            suggestion: Some(
                "Create .gitignore with: .pledge/ node_modules/ .env.local".to_string(),
            ),
        });
    }

    checks
}

/// On Windows, Git Bash's coreutils `link.exe` shadows the MSVC linker when
/// `Git\usr\bin` precedes it in PATH — `cargo build` then fails with the
/// cryptic `link: extra operand ...` (the Unix link(1) tool, not the linker).
/// Only relevant when rustc's host toolchain is the MSVC triple.
#[cfg(windows)]
fn check_toolchain() -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    let host_is_msvc = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .map(|v| {
            v.lines()
                .find(|l| l.starts_with("host:"))
                .is_some_and(|l| l.contains("msvc"))
        })
        .unwrap_or(false);
    if !host_is_msvc {
        return checks;
    }

    if let Ok(out) = std::process::Command::new("where").arg("link.exe").output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(first) = stdout.lines().next()
            && first.to_lowercase().contains("git")
        {
            checks.push(DiagnosticCheck {
                category: DiagnosticCategory::Project,
                status: DiagnosticStatus::Warn,
                name: "Linker shadowing".to_string(),
                message: format!(
                    "`link.exe` resolves to Git's coreutils tool ({first}), not the MSVC linker"
                ),
                suggestion: Some(
                    "Build with `cargo +stable-x86_64-pc-windows-gnu`, or put MSVC's \
                     link.exe ahead of Git\\usr\\bin in PATH"
                        .to_string(),
                ),
            });
        }
    }
    checks
}

#[cfg(not(windows))]
fn check_toolchain() -> Vec<DiagnosticCheck> {
    Vec::new()
}

fn check_security(root: &Path) -> Vec<DiagnosticCheck> {
    let mut checks = Vec::new();

    // Check if .env is gitignored
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    if root.join(".env").exists() && !gitignore.contains(".env") {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Security,
            status: DiagnosticStatus::Fail,
            name: ".env in git".to_string(),
            message: ".env file exists but is not in .gitignore".to_string(),
            suggestion: Some(
                "Add .env to .gitignore immediately to prevent committing secrets".to_string(),
            ),
        });
    }

    // Check for .env.local in git
    if root.join(".env.local").exists() && !gitignore.contains(".env.local") {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Security,
            status: DiagnosticStatus::Fail,
            name: ".env.local in git".to_string(),
            message: ".env.local exists but is not gitignored".to_string(),
            suggestion: Some("Add .env.local to .gitignore".to_string()),
        });
    }

    // Supply-chain pass: known-vulnerable dependencies.
    // 1. Offline advisory database — always runs, no network needed.
    let vulns = crate::security::scan_vulnerabilities(root);
    for v in &vulns {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Security,
            status: match v.severity {
                crate::security::VulnerabilitySeverity::Critical
                | crate::security::VulnerabilitySeverity::High => DiagnosticStatus::Fail,
                _ => DiagnosticStatus::Warn,
            },
            name: format!("Vulnerable dep: {}", v.package),
            message: format!(
                "{}@{} — {} [{}]{}",
                v.package,
                v.version,
                v.title,
                v.severity.label(),
                v.cve.as_ref().map(|c| format!(" ({c})")).unwrap_or_default()
            ),
            suggestion: v
                .patch_version
                .as_ref()
                .map(|p| format!("Upgrade to {}@{}", v.package, p)),
        });
    }

    // 2. `npm audit --json` — live registry data, best-effort. Skipped
    //    silently when npm or a lockfile is absent; audit failures degrade
    //    to Info rather than failing a diagnostics pass over a network blip.
    if root.join("package-lock.json").exists()
        && let Ok(out) = std::process::Command::new("npm")
            .args(["audit", "--json", "--omit=dev"])
            .current_dir(root)
            .output()
    {
        // npm exits non-zero when vulns exist — parse stdout regardless.
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            && let Some(meta) = json
                .get("metadata")
                .and_then(|m| m.get("vulnerabilities"))
        {
            let total: u64 = ["critical", "high", "moderate", "low"]
                .iter()
                .filter_map(|k| meta.get(*k).and_then(|v| v.as_u64()))
                .sum();
            if total > 0 {
                let critical_high = meta
                    .get("critical")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    + meta.get("high").and_then(|v| v.as_u64()).unwrap_or(0);
                checks.push(DiagnosticCheck {
                    category: DiagnosticCategory::Security,
                    status: if critical_high > 0 {
                        DiagnosticStatus::Fail
                    } else {
                        DiagnosticStatus::Warn
                    },
                    name: "npm audit".to_string(),
                    message: format!(
                        "npm audit: {} vulnerabilities ({} critical, {} high)",
                        total,
                        meta.get("critical").and_then(|v| v.as_u64()).unwrap_or(0),
                        meta.get("high").and_then(|v| v.as_u64()).unwrap_or(0),
                    ),
                    suggestion: Some("Run `npm audit` for details, `npm audit fix` to remediate".to_string()),
                });
            } else {
                checks.push(DiagnosticCheck {
                    category: DiagnosticCategory::Security,
                    status: DiagnosticStatus::Pass,
                    name: "npm audit".to_string(),
                    message: "npm audit: no known vulnerabilities".to_string(),
                    suggestion: None,
                });
            }
        }
    }

    // Check for sensitive patterns in config
    for config_file in [
        "pledge.config.ts",
        "pledge.config.js",
        "pledge.config.json",
        "pledge.json",
    ] {
        let path = root.join(config_file);
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            for pattern in ["API_KEY", "SECRET", "PRIVATE_KEY", "PASSWORD", "TOKEN"] {
                if content.contains(pattern) {
                    checks.push(DiagnosticCheck {
                        category: DiagnosticCategory::Security,
                        status: DiagnosticStatus::Warn,
                        name: "Secrets in config".to_string(),
                        message: format!("Possible secret '{}' found in {}", pattern, config_file),
                        suggestion: Some(
                            "Move secrets to .env files and reference via import.meta.env"
                                .to_string(),
                        ),
                    });
                    break;
                }
            }
        }
    }

    if checks.is_empty() {
        checks.push(DiagnosticCheck {
            category: DiagnosticCategory::Security,
            status: DiagnosticStatus::Pass,
            name: "Security scan".to_string(),
            message: "No security issues found".to_string(),
            suggestion: None,
        });
    }

    checks
}
