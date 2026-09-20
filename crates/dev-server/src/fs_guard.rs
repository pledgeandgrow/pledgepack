//! Filesystem access policy for the dev server's file-serving routes.
//!
//! Every route that reads a file named by the request (`/{*path}`, `/@fs/`,
//! `/@id/`, `/__pledge_public/`) goes through [`resolve_servable`], which
//!
//! 1. rejects Windows tricks lexically (alternate data streams `file::$DATA`,
//!    trailing dots/spaces, NUL bytes),
//! 2. canonicalizes the target (resolving symlinks, `..`, case differences and
//!    8.3 short names to the real on-disk path),
//! 3. requires the canonical path to sit inside an *allowed root* (the project
//!    root plus any roots listed in `PLEDGE_DEV_FS_ALLOW`, like Vite's
//!    `server.fs.allow`),
//! 4. applies a deny-list of secret-bearing names (`.env*`, dotfiles, private
//!    keys, credentials, the pledge config) to both the requested and the
//!    canonical path, so neither a symlink *to* a secret nor a secret reached
//!    through a differently-cased/short name is served, and
//! 5. only serves files with a module-like or static-asset extension.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Environment variable listing extra allowed roots (OS path-list separated).
pub const FS_ALLOW_ENV: &str = "PLEDGE_DEV_FS_ALLOW";

/// Why a path was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// The file does not exist (or cannot be resolved).
    NotFound,
    /// The path is outside the allowed roots or names a protected file.
    Forbidden,
}

/// Extensions the dev server will serve: module-like sources and static assets.
const ALLOWED_EXTENSIONS: &[&str] = &[
    // modules / sources
    "js", "mjs", "cjs", "jsx", "ts", "tsx", "mts", "cts", "css", "scss", "sass", "less", "styl",
    "json", "vue", "svelte", "astro", "html", "htm", "md", "mdx", "wasm", "map", // assets
    "png", "jpg", "jpeg", "gif", "svg", "webp", "avif", "ico", "bmp", "woff", "woff2", "ttf",
    "otf", "eot", "mp4", "webm", "ogg", "mp3", "wav", "flac", "pdf", "txt", "xml", "csv",
];

/// File names (lowercased) that never get served.
const DENIED_NAMES: &[&str] = &[
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "secrets.json",
    "secret.json",
    "credentials.json",
    "credentials",
    "service-account.json",
    "serviceaccount.json",
    "auth.json",
    "htpasswd",
    "shadow",
    "passwd",
];

/// Extensions (lowercased) of private key / keystore material.
const DENIED_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "keystore", "jks", "asc", "gpg", "kdbx", "ppk", "crt", "cer",
];

/// Project manifests / lockfiles: never useful to a browser as plain files and
/// they leak the dependency tree. `package.json` may still be *imported as a
/// module* (`import pkg from './package.json'`); lockfiles never are.
const MANIFEST_NAMES: &[&str] = &["package.json"];
const LOCKFILE_NAMES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
];

fn file_name_lower(path: &Path) -> Option<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
}

/// True for `package.json`.
pub fn is_manifest(path: &Path) -> bool {
    file_name_lower(path).is_some_and(|n| MANIFEST_NAMES.contains(&n.as_str()))
}

/// True for package-manager lockfiles.
pub fn is_lockfile(path: &Path) -> bool {
    file_name_lower(path).is_some_and(|n| LOCKFILE_NAMES.contains(&n.as_str()))
}

/// Returns true if a single path component (a file or directory name) is
/// protected. `parent` is the previous component, used to allow pnpm's
/// `node_modules/.pnpm` store.
fn component_denied(name: &str, parent: Option<&str>) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with('.') {
        // node_modules/.pnpm holds real package files under pnpm layouts.
        if lower == ".pnpm" && parent.is_some_and(|p| p.eq_ignore_ascii_case("node_modules")) {
            return false;
        }
        return true;
    }
    if DENIED_NAMES.contains(&lower.as_str()) {
        return true;
    }
    if lower.starts_with("pledge.config.") || lower.starts_with("pledgepack.config.") {
        return true;
    }
    if let Some(ext) = lower.rsplit_once('.').map(|(_, e)| e)
        && DENIED_EXTENSIONS.contains(&ext)
    {
        return true;
    }
    false
}

/// Lexical check of a (root-relative or absolute) path's components against
/// the deny-list. Case-insensitive on every platform.
pub fn path_is_denied(path: &Path) -> bool {
    let mut parent: Option<String> = None;
    for comp in path.components() {
        if let Component::Normal(name) = comp {
            let name = name.to_string_lossy();
            if component_denied(&name, parent.as_deref()) {
                return true;
            }
            parent = Some(name.into_owned());
        }
    }
    false
}

/// True if the file's extension is one the dev server is willing to serve.
pub fn extension_allowed(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| ALLOWED_EXTENSIONS.contains(&e.as_str()))
}

/// Windows-style path tricks that must never reach the filesystem, checked on
/// every platform so behaviour does not depend on the host OS. `raw` is the
/// request-derived portion (may include a drive prefix for `/@fs/C:/...`).
fn lexically_suspicious(raw: &str) -> bool {
    if raw.contains('\0') {
        return true;
    }
    // Strip a leading drive prefix ("C:") before looking for stream colons.
    let b = raw.as_bytes();
    let rest = if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        &raw[2..]
    } else {
        raw
    };
    if rest.contains(':') {
        return true; // NTFS alternate data stream (`.env::$DATA`) or device path
    }
    rest.split(['/', '\\']).any(|seg| {
        // "." and ".." are handled by canonicalization; a trailing dot or
        // space on any other segment is silently trimmed by Windows, letting
        // `.env.` reach `.env`.
        seg != "." && seg != ".." && !seg.is_empty() && (seg.ends_with('.') || seg.ends_with(' '))
    })
}

/// The allowed roots: canonical project root plus `PLEDGE_DEV_FS_ALLOW` entries.
fn allowed_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(r) = std::fs::canonicalize(root) {
        roots.push(r);
    }
    if let Some(extra) = std::env::var_os(FS_ALLOW_ENV) {
        for p in std::env::split_paths(&extra) {
            if let Ok(c) = std::fs::canonicalize(&p) {
                roots.push(c);
            }
        }
    }
    roots
}

/// Resolve `candidate` (an already-joined filesystem path derived from
/// `request_path`) to a canonical file path that is safe to serve.
///
/// `request_path` is the raw, request-derived string, used for the lexical
/// Windows-trick checks. `candidate` must exist and be a regular file.
pub fn resolve_servable(
    root: &Path,
    request_path: &str,
    candidate: &Path,
) -> Result<PathBuf, Denied> {
    resolve_servable_with(root, request_path, candidate, false)
}

/// [`resolve_servable`] for a request that is an ES module import
/// (`?import`, or a browser `Sec-Fetch-Dest: script` fetch): `package.json`
/// may be imported as a module. Lockfiles are still refused.
pub fn resolve_servable_module(
    root: &Path,
    request_path: &str,
    candidate: &Path,
) -> Result<PathBuf, Denied> {
    resolve_servable_with(root, request_path, candidate, true)
}

fn resolve_servable_with(
    root: &Path,
    request_path: &str,
    candidate: &Path,
    allow_manifest: bool,
) -> Result<PathBuf, Denied> {
    if lexically_suspicious(request_path) {
        return Err(Denied::Forbidden);
    }
    let canonical = std::fs::canonicalize(candidate).map_err(|_| Denied::NotFound)?;
    if !canonical.is_file() {
        return Err(Denied::NotFound);
    }
    let roots = allowed_roots(root);
    let Some(matched_root) = roots.iter().find(|r| canonical.starts_with(r)) else {
        return Err(Denied::Forbidden);
    };

    // Deny-list applies to the request as written (catches a secret-named
    // symlink) and to the resolved real path (catches a symlink, short name or
    // case variant that resolves to a secret).
    let rel_canonical = canonical
        .strip_prefix(matched_root)
        .unwrap_or(canonical.as_path());
    if path_is_denied(rel_canonical) {
        return Err(Denied::Forbidden);
    }
    let rel_request = candidate.strip_prefix(root).unwrap_or(candidate);
    if path_is_denied(rel_request) {
        return Err(Denied::Forbidden);
    }
    if !extension_allowed(&canonical) {
        return Err(Denied::Forbidden);
    }
    // Manifests/lockfiles, checked on both the requested and the real name.
    for p in [candidate, canonical.as_path()] {
        if is_lockfile(p) || (!allow_manifest && is_manifest(p)) {
            return Err(Denied::Forbidden);
        }
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_dotfiles_and_secrets() {
        for p in [
            ".env",
            ".ENV.local",
            "sub/.env.production",
            ".git/config",
            "certs/server.pem",
            "certs/SERVER.KEY",
            "id_rsa",
            "secrets.json",
            "pledge.config.ts",
            "a/.ssh/known_hosts",
        ] {
            assert!(path_is_denied(Path::new(p)), "{p} should be denied");
        }
    }

    #[test]
    fn allows_normal_and_pnpm_paths() {
        for p in [
            "src/main.tsx",
            "node_modules/react/index.js",
            "node_modules/.pnpm/react@18.0.0/node_modules/react/index.js",
            "public/logo.svg",
        ] {
            assert!(!path_is_denied(Path::new(p)), "{p} should be allowed");
        }
        // .pnpm outside node_modules is a plain dotdir
        assert!(path_is_denied(Path::new("src/.pnpm/x.js")));
    }

    #[test]
    fn manifests_and_lockfiles_are_denied_to_plain_fetches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        for name in [
            "package.json",
            "package-lock.json",
            "yarn.lock",
            "pnpm-lock.yaml",
            "npm-shrinkwrap.json",
        ] {
            std::fs::write(root.join(name), "{}").unwrap();
        }
        std::fs::write(root.join("src/data.json"), "{}").unwrap();

        // package.json: refused as a plain file, allowed as a module import.
        let pj = root.join("package.json");
        assert_eq!(resolve_servable(&root, "package.json", &pj), Err(Denied::Forbidden));
        assert!(resolve_servable_module(&root, "package.json", &pj).is_ok());
        // Case variants resolve to the same file and are refused too.
        let upper = root.join("PACKAGE.JSON");
        if upper.exists() {
            assert_eq!(resolve_servable(&root, "PACKAGE.JSON", &upper), Err(Denied::Forbidden));
        }
        // Lockfiles: never (either mode). Only the JSON ones pass the extension
        // allowlist, the YAML/.lock ones are refused by it as well.
        for name in ["package-lock.json", "yarn.lock", "pnpm-lock.yaml", "npm-shrinkwrap.json"] {
            let p = root.join(name);
            assert_eq!(resolve_servable(&root, name, &p), Err(Denied::Forbidden), "{name}");
            assert_eq!(
                resolve_servable_module(&root, name, &p),
                Err(Denied::Forbidden),
                "{name} (module)"
            );
        }
        // Ordinary JSON stays servable.
        assert!(resolve_servable(&root, "src/data.json", &root.join("src/data.json")).is_ok());
    }

    #[test]
    fn extension_allowlist() {
        assert!(extension_allowed(Path::new("a.tsx")));
        assert!(extension_allowed(Path::new("a.PNG")));
        assert!(!extension_allowed(Path::new("Cargo.toml")));
        assert!(!extension_allowed(Path::new("db.sqlite")));
        assert!(!extension_allowed(Path::new("noext")));
    }

    #[test]
    fn windows_tricks_are_suspicious() {
        assert!(lexically_suspicious(".env::$DATA"));
        assert!(lexically_suspicious("src/main.js:secret"));
        assert!(lexically_suspicious(".env."));
        assert!(lexically_suspicious("src/a.js "));
        assert!(lexically_suspicious("a\0.js"));
        assert!(!lexically_suspicious("C:/proj/src/a.js"));
        assert!(!lexically_suspicious("src/../src/a.js"));
        assert!(!lexically_suspicious("src/a.js"));
    }

    #[test]
    fn resolve_rejects_secret_outside_and_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::write(root.join("src/a.js"), "1").unwrap();
        std::fs::write(tmp.path().join("outside.js"), "1").unwrap();

        let ok = root.join("src/a.js");
        assert!(resolve_servable(&root, "src/a.js", &ok).is_ok());
        assert_eq!(
            resolve_servable(&root, ".env", &root.join(".env")),
            Err(Denied::Forbidden)
        );
        assert_eq!(
            resolve_servable(&root, "../outside.js", &root.join("../outside.js")),
            Err(Denied::Forbidden)
        );
        assert_eq!(
            resolve_servable(&root, "src/missing.js", &root.join("src/missing.js")),
            Err(Denied::NotFound)
        );
        // symlink named like a module but pointing at a secret / outside file
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join(".env"), root.join("src/link.js")).unwrap();
            assert_eq!(
                resolve_servable(&root, "src/link.js", &root.join("src/link.js")),
                Err(Denied::Forbidden)
            );
            std::os::unix::fs::symlink(tmp.path().join("outside.js"), root.join("src/out.js"))
                .unwrap();
            assert_eq!(
                resolve_servable(&root, "src/out.js", &root.join("src/out.js")),
                Err(Denied::Forbidden)
            );
        }
    }
}
