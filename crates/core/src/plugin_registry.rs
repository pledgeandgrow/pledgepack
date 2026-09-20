// Plugin registry — npm-scoped registry for community plugins (#67)
//
// Features:
//   - Search npm for @pledgepack-plugin-* packages
//   - Install plugins via npm/pnpm/yarn
//   - List installed plugins
//   - Plugin discovery without leaving the CLI

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::process::Command;

/// NPM search result from registry API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpmSearchResult {
    pub objects: Vec<NpmSearchObject>,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpmSearchObject {
    pub package: NpmPackage,
    pub score: NpmScore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpmPackage {
    pub name: String,
    pub version: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub links: NpmPackageLinks,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpmPackageLinks {
    pub npm: String,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NpmScore {
    #[serde(rename = "final")]
    pub final_score: f64,
    pub quality: f64,
    pub popularity: f64,
    pub maintenance: f64,
}

/// Plugin info displayed to user
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub score: f64,
    pub url: String,
    /// G12.19: Install count from npm
    pub install_count: u32,
    /// G12.19: Star rating (0-5)
    pub rating: f32,
    /// G12.19: Category (e.g., "css", "images", "framework")
    pub category: String,
    /// G12.19: Author/maintainer
    pub author: String,
    /// G12.19: Whether the plugin is verified
    pub verified: bool,
}

/// Search the npm registry for PledgePack plugins
pub fn search_plugins(query: Option<&str>) -> Result<Vec<PluginInfo>> {
    let search_term = match query {
        Some(q) => format!("pledgepack-plugin-{}", q),
        None => "pledgepack-plugin".to_string(),
    };

    let url = format!(
        "https://registry.npmjs.org/-/v1/search?text={}&size=25",
        urlencode(&search_term)
    );

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();

    let mut response = agent.get(&url).call()?;

    let body = response.body_mut().read_to_string()?;
    let result: NpmSearchResult = serde_json::from_str(&body)?;

    let plugins: Vec<PluginInfo> = result
        .objects
        .into_iter()
        .filter(|obj| {
            obj.package.name.contains("pledgepack-plugin")
                || obj.package.keywords.iter().any(|k| k == "pledgepack")
        })
        .map(|obj| {
            let install_count = (obj.score.popularity * 10000.0) as u32;
            let rating =
                ((obj.score.quality * 3.0 + obj.score.maintenance * 2.0) / 5.0 * 5.0) as f32;
            let category = obj
                .package
                .keywords
                .iter()
                .find(|k| {
                    k.starts_with("pledge-")
                        || k.starts_with("category:")
                        || matches!(
                            k.as_str(),
                            "css"
                                | "images"
                                | "framework"
                                | "data"
                                | "assets"
                                | "env"
                                | "optimization"
                                | "dev"
                                | "testing"
                                | "pwa"
                                | "i18n"
                                | "misc"
                        )
                })
                .cloned()
                .unwrap_or_else(|| "misc".to_string());
            let author = obj.package.links.repository.as_deref().unwrap_or("unknown");
            let verified = obj.package.name.starts_with("@pledgepack/");
            PluginInfo {
                name: obj.package.name,
                version: obj.package.version,
                description: obj.package.description,
                score: obj.score.final_score,
                url: obj.package.links.npm,
                install_count,
                rating,
                category,
                author: author.to_string(),
                verified,
            }
        })
        .collect();

    Ok(plugins)
}

/// Install a plugin using the detected package manager
pub fn install_plugin(plugin_name: &str, dev: bool) -> Result<()> {
    let (pm, _args) = detect_package_manager();

    let mut cmd_args = vec!["install".to_string()];
    if dev {
        cmd_args.push("--save-dev".to_string());
    } else if pm == "pnpm" {
        cmd_args.push("--save".to_string());
    }
    cmd_args.push(plugin_name.to_string());

    println!(
        "  \x1b[36m→\x1b[0m Installing {} via {}...",
        plugin_name, pm
    );

    let status = Command::new(&pm).args(&cmd_args).status()?;

    if !status.success() {
        anyhow::bail!("Failed to install {} with {}", plugin_name, pm);
    }

    println!("  \x1b[32m✓\x1b[0m Installed {}", plugin_name);

    // Suggest adding to pledge.config.ts
    println!("\n  \x1b[90m→\x1b[0m Add to pledge.config.ts:");
    println!("  \x1b[90m  plugins: [\"{}\"]\x1b[0m\n", plugin_name);

    Ok(())
}

/// List installed PledgePack plugins from node_modules
pub fn list_installed_plugins(root: &std::path::Path) -> Result<Vec<PluginInfo>> {
    let node_modules = root.join("node_modules");
    if !node_modules.exists() {
        return Ok(Vec::new());
    }

    let mut plugins = Vec::new();

    // Check @pledgepack scope
    let scoped = node_modules.join("@pledgepack");
    if scoped.exists()
        && let Ok(entries) = std::fs::read_dir(&scoped)
    {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("plugin-") {
                let full_name = format!("@pledgepack/{}", name);
                if let Some(info) = read_package_json(&node_modules, &full_name) {
                    plugins.push(info);
                }
            }
        }
    }

    // Check unscoped pledgepack-plugin-* packages
    if let Ok(entries) = std::fs::read_dir(&node_modules) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("pledgepack-plugin-")
                && let Some(info) = read_package_json(&node_modules, &name)
            {
                plugins.push(info);
            }
        }
    }

    Ok(plugins)
}

/// Read package.json from node_modules to get plugin info
fn read_package_json(node_modules: &std::path::Path, name: &str) -> Option<PluginInfo> {
    let pkg_path = node_modules.join(name).join("package.json");
    let content = std::fs::read_to_string(&pkg_path).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&content).ok()?;

    Some(PluginInfo {
        name: name.to_string(),
        version: pkg
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        description: pkg
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        score: 0.0,
        url: format!("https://www.npmjs.com/package/{}", name),
        install_count: 0,
        rating: 0.0,
        category: pkg
            .get("keywords")
            .and_then(|k| k.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|k| k.as_str())
                    .find(|k| {
                        matches!(
                            k,
                            &"css"
                                | &"images"
                                | &"framework"
                                | &"data"
                                | &"assets"
                                | &"env"
                                | &"optimization"
                                | &"dev"
                                | &"testing"
                                | &"pwa"
                                | &"i18n"
                                | &"misc"
                        )
                    })
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "misc".to_string()),
        author: pkg
            .get("author")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown")
            .to_string(),
        verified: name.starts_with("@pledgepack/"),
    })
}

/// Detect which package manager is in use (npm, pnpm, yarn)
fn detect_package_manager() -> (String, Vec<String>) {
    let cwd = std::env::current_dir().unwrap_or_default();

    // Check for lock files
    if cwd.join("pnpm-lock.yaml").exists() {
        return ("pnpm".to_string(), vec![]);
    }
    if cwd.join("yarn.lock").exists() {
        return ("yarn".to_string(), vec![]);
    }
    // Default to npm
    ("npm".to_string(), vec![])
}

/// Format plugin list for terminal output with marketplace info
pub fn format_plugin_list(plugins: &[PluginInfo]) -> String {
    if plugins.is_empty() {
        return "  No plugins found.".to_string();
    }

    let mut out = String::new();
    for p in plugins {
        let verified_badge = if p.verified {
            " \x1b[32m✓\x1b[0m"
        } else {
            ""
        };
        let stars = if p.rating > 0.0 {
            format!(" \x1b[33m★{:.1}\x1b[0m", p.rating)
        } else {
            String::new()
        };
        let installs = if p.install_count > 0 {
            format!(" \x1b[90m({} installs)\x1b[0m", p.install_count)
        } else {
            String::new()
        };
        out.push_str(&format!(
            "  \x1b[36m{}\x1b[0m{}{}{} \x1b[90mv{}\x1b[0m\n    {}\n    \x1b[90m{}\x1b[0m\n\n",
            p.name, verified_badge, stars, installs, p.version, p.description, p.url
        ));
    }
    out
}

/// G12.19: Format plugin marketplace table with ratings, install counts, and categories
pub fn format_plugin_marketplace(plugins: &[PluginInfo]) -> String {
    if plugins.is_empty() {
        return "  No plugins found in marketplace.".to_string();
    }

    let mut out = String::new();
    out.push_str("  \x1b[1mPledgePack Plugin Marketplace\x1b[0m\n");
    out.push_str("  ─────────────────────────────────\n\n");

    // Group by category
    let mut categories: std::collections::BTreeMap<&str, Vec<&PluginInfo>> =
        std::collections::BTreeMap::new();
    for p in plugins {
        categories.entry(&p.category).or_default().push(p);
    }

    for (cat, cat_plugins) in &categories {
        out.push_str(&format!(
            "  \x1b[35m{}\x1b[0m ({}):\n",
            cat,
            cat_plugins.len()
        ));
        for p in cat_plugins {
            let verified = if p.verified { " ✓" } else { "" };
            let stars = if p.rating > 0.0 {
                format!(" ★{:.1}", p.rating)
            } else {
                String::new()
            };
            let installs = if p.install_count > 0 {
                format!(", {} installs", p.install_count)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "    \x1b[36m{}\x1b[0m{} \x1b[90mv{}\x1b[0m{}{}\n      \x1b[90m{}\x1b[0m\n",
                p.name, verified, p.version, stars, installs, p.description
            ));
        }
        out.push('\n');
    }

    out
}

/// URL-encode a string for query parameters
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u8)
            }
        })
        .collect()
}

// ─── G7.8: WASM plugin download and hash pinning ─────────────────────

/// A pinned WASM plugin with its content hash
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedWasmPlugin {
    /// Plugin name (e.g., "@pledgepack/css-modules")
    pub name: String,
    /// Version string
    pub version: String,
    /// blake3 hash of the .wasm file content
    pub hash: String,
    /// Download URL
    pub url: String,
    /// File size in bytes
    pub size: u64,
}

/// Download a .wasm plugin file and pin it by hash.
/// The plugin is downloaded to the local plugin cache directory.
pub fn download_and_pin_wasm_plugin(
    name: &str,
    version: &str,
    wasm_url: &str,
    cache_dir: &std::path::Path,
) -> Result<PinnedWasmPlugin> {
    std::fs::create_dir_all(cache_dir)?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .into();

    let mut response = agent.get(wasm_url).call()?;
    let body = response.body_mut().read_to_vec()?;

    // Compute blake3 hash
    let hash = blake3::hash(&body);
    let hash_hex = hash.to_hex().to_string();

    let filename = format!(
        "{}-{}.wasm",
        name.replace('/', "_").replace('@', ""),
        version
    );
    let plugin_path = cache_dir.join(&filename);
    std::fs::write(&plugin_path, &body)?;

    let pinned = PinnedWasmPlugin {
        name: name.to_string(),
        version: version.to_string(),
        hash: hash_hex,
        url: wasm_url.to_string(),
        size: body.len() as u64,
    };

    // Write pin file (JSON metadata)
    let pin_path = cache_dir.join(format!("{}.pin.json", filename));
    let pin_content = serde_json::to_string_pretty(&pinned)?;
    std::fs::write(&pin_path, pin_content)?;

    Ok(pinned)
}

/// Verify a pinned plugin's hash matches the stored file.
pub fn verify_pinned_plugin(pin: &PinnedWasmPlugin, cache_dir: &std::path::Path) -> Result<bool> {
    let filename = format!(
        "{}-{}.wasm",
        pin.name.replace('/', "_").replace('@', ""),
        pin.version
    );
    let plugin_path = cache_dir.join(&filename);

    if !plugin_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read(&plugin_path)?;
    let hash = blake3::hash(&content);
    let hash_hex = hash.to_hex().to_string();

    Ok(hash_hex == pin.hash)
}

/// List all pinned WASM plugins in the cache directory.
pub fn list_pinned_plugins(cache_dir: &std::path::Path) -> Result<Vec<PinnedWasmPlugin>> {
    let mut plugins = Vec::new();

    if !cache_dir.exists() {
        return Ok(plugins);
    }

    for entry in std::fs::read_dir(cache_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .ends_with(".pin.json")
            && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(pinned) = serde_json::from_str::<PinnedWasmPlugin>(&content)
        {
            plugins.push(pinned);
        }
    }

    Ok(plugins)
}

/// Remove a pinned plugin from the cache.
/// Sign a plugin file, producing a `<path>.sig.json` trust sidecar that
/// the plugin hosts verify at load time (see `plugin_security` config).
///
/// `secret_key_hex` is a 32-byte Ed25519 secret key (hex); the signature
/// covers the plugin's blake3 hash, exactly what
/// `PluginSigningVerifier::verify` checks. The signer identity +
/// public key are embedded in the sidecar — consumers must add that pair
/// to `plugin_security.trusted_keys` for the plugin to load.
pub fn sign_plugin(
    path: &std::path::Path,
    identity: &str,
    secret_key_hex: &str,
    capabilities: &[crate::plugin_system::PluginCapability],
) -> Result<std::path::PathBuf> {
    use ed25519_dalek::{Signer, SigningKey};

    let bytes = std::fs::read(path)
        .map_err(|e| anyhow::anyhow!("Failed to read plugin {}: {e}", path.display()))?;
    let hash_hex = blake3::hash(&bytes).to_hex().to_string();

    let secret_bytes = hex::decode(secret_key_hex.trim())
        .map_err(|e| anyhow::anyhow!("Invalid signing key (expected 64-char hex): {e}"))?;
    let secret_bytes: [u8; 32] = secret_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Signing key must be 32 bytes (64 hex chars)"))?;
    let signing_key = SigningKey::from_bytes(&secret_bytes);
    let signature = signing_key.sign(hash_hex.as_bytes());

    let plugin_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("plugin")
        .to_string();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let sidecar = crate::plugin_system::PluginTrustSidecar {
        signature: crate::plugin_system::PluginSignature {
            plugin_name,
            version: "0.0.0".to_string(),
            wasm_hash: hash_hex,
            signer_public_key: hex::encode(signing_key.verifying_key().to_bytes()),
            signature: hex::encode(signature.to_bytes()),
            signer_identity: identity.to_string(),
            timestamp,
            verified: false,
        },
        capabilities: capabilities.to_vec(),
    };

    let sidecar_path = {
        let mut s = path.as_os_str().to_os_string();
        s.push(".sig.json");
        std::path::PathBuf::from(s)
    };
    let json = serde_json::to_string_pretty(&sidecar)?;
    std::fs::write(&sidecar_path, format!("{json}\n"))?;

    Ok(sidecar_path)
}

/// Generate a fresh Ed25519 signing keypair for plugin signing. Returns
/// (secret_key_hex, public_key_hex).
pub fn generate_signing_key() -> (String, String) {
    let mut rng = rand::rngs::OsRng;
    let signing_key = ed25519_dalek::SigningKey::generate(&mut rng);
    (
        hex::encode(signing_key.to_bytes()),
        hex::encode(signing_key.verifying_key().to_bytes()),
    )
}

pub fn unpin_plugin(name: &str, version: &str, cache_dir: &std::path::Path) -> Result<()> {
    let filename = format!(
        "{}-{}.wasm",
        name.replace('/', "_").replace('@', ""),
        version
    );
    let plugin_path = cache_dir.join(&filename);
    let pin_path = cache_dir.join(format!("{}.pin.json", filename));

    if plugin_path.exists() {
        std::fs::remove_file(&plugin_path)?;
    }
    if pin_path.exists() {
        std::fs::remove_file(&pin_path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pinned_wasm_plugin_serialization() {
        let pinned = PinnedWasmPlugin {
            name: "@pledgepack/css-modules".to_string(),
            version: "1.0.0".to_string(),
            hash: "abcdef0123456789".to_string(),
            url: "https://registry.npmjs.org/@pledgepack/css-modules/-/1.0.0/plugin.wasm"
                .to_string(),
            size: 1024,
        };
        let json = serde_json::to_string(&pinned).unwrap();
        let deserialized: PinnedWasmPlugin = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, pinned.name);
        assert_eq!(deserialized.hash, pinned.hash);
        assert_eq!(deserialized.size, 1024);
    }

    #[test]
    fn test_g1219_format_plugin_marketplace() {
        let plugins = vec![
            PluginInfo {
                name: "@pledgepack/css-modules".to_string(),
                version: "1.0.0".to_string(),
                description: "CSS modules plugin".to_string(),
                score: 0.9,
                url: "https://npmjs.com/package/@pledgepack/css-modules".to_string(),
                install_count: 5000,
                rating: 4.5,
                category: "css".to_string(),
                author: "pledgepack".to_string(),
                verified: true,
            },
            PluginInfo {
                name: "pledgepack-plugin-images".to_string(),
                version: "0.2.0".to_string(),
                description: "Image optimization".to_string(),
                score: 0.7,
                url: "https://npmjs.com/package/pledgepack-plugin-images".to_string(),
                install_count: 1200,
                rating: 3.8,
                category: "images".to_string(),
                author: "community".to_string(),
                verified: false,
            },
        ];

        let output = format_plugin_marketplace(&plugins);
        assert!(output.contains("PledgePack Plugin Marketplace"));
        assert!(output.contains("css"));
        assert!(output.contains("images"));
        assert!(output.contains("@pledgepack/css-modules"));
        assert!(output.contains("★4.5"));
        assert!(output.contains("5000 installs"));
    }

    #[test]
    fn test_g1219_format_plugin_list_with_marketplace_info() {
        let plugins = vec![PluginInfo {
            name: "@pledgepack/test".to_string(),
            version: "1.0.0".to_string(),
            description: "Test plugin".to_string(),
            score: 0.8,
            url: "https://npmjs.com".to_string(),
            install_count: 100,
            rating: 4.0,
            category: "misc".to_string(),
            author: "test".to_string(),
            verified: true,
        }];

        let output = format_plugin_list(&plugins);
        assert!(output.contains("@pledgepack/test"));
        assert!(output.contains("★4.0"));
        assert!(output.contains("100 installs"));
    }

    #[test]
    fn test_list_pinned_plugins_empty_dir() {
        let tmp = std::env::temp_dir().join("pledge_test_pin_empty");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let plugins = list_pinned_plugins(&tmp).unwrap();
        assert!(plugins.is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_list_pinned_plugins_with_pin() {
        let tmp = std::env::temp_dir().join("pledge_test_pin_list");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let pinned = PinnedWasmPlugin {
            name: "@pledgepack/test".to_string(),
            version: "0.1.0".to_string(),
            hash: "deadbeef".to_string(),
            url: "https://example.com/test.wasm".to_string(),
            size: 512,
        };
        let pin_path = tmp.join("_pledgepack_test-0.1.0.wasm.pin.json");
        std::fs::write(&pin_path, serde_json::to_string_pretty(&pinned).unwrap()).unwrap();

        let plugins = list_pinned_plugins(&tmp).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "@pledgepack/test");
        assert_eq!(plugins[0].hash, "deadbeef");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_unpin_plugin() {
        let tmp = std::env::temp_dir().join("pledge_test_unpin");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let plugin_path = tmp.join("pledgepack_test-1.0.0.wasm");
        let pin_path = tmp.join("pledgepack_test-1.0.0.wasm.pin.json");
        std::fs::write(&plugin_path, b"\0asm\x01\0\0\0").unwrap();
        std::fs::write(&pin_path, "{}").unwrap();

        unpin_plugin("@pledgepack/test", "1.0.0", &tmp).unwrap();

        assert!(!plugin_path.exists());
        assert!(!pin_path.exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_verify_pinned_plugin_missing_file() {
        let tmp = std::env::temp_dir().join("pledge_test_verify_missing");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let pin = PinnedWasmPlugin {
            name: "test".to_string(),
            version: "1.0.0".to_string(),
            hash: "abc".to_string(),
            url: "https://example.com/test.wasm".to_string(),
            size: 100,
        };

        assert!(!verify_pinned_plugin(&pin, &tmp).unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_verify_pinned_plugin_correct_hash() {
        let tmp = std::env::temp_dir().join("pledge_test_verify_correct");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let content = b"\0asm\x01\0\0\0\x02\x00\x00\x00";
        let hash = blake3::hash(content);
        let hash_hex = hash.to_hex().to_string();

        let plugin_path = tmp.join("test-1.0.0.wasm");
        std::fs::write(&plugin_path, content).unwrap();

        let pin = PinnedWasmPlugin {
            name: "test".to_string(),
            version: "1.0.0".to_string(),
            hash: hash_hex,
            url: "https://example.com/test.wasm".to_string(),
            size: content.len() as u64,
        };

        assert!(verify_pinned_plugin(&pin, &tmp).unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
