// Git-based cache invalidation
//
// Uses git tree hashes instead of file content hashes for faster
// cache invalidation on large repos. A single `git ls-files` + hash
// is much faster than reading every file to compute blake3 hashes.
//
// Strategy:
//   1. `git ls-files -s` gives file paths + blob hashes in one command
//   2. The git tree hash for a directory captures the entire state
//   3. If the tree hash hasn't changed, all files under it are unchanged
//   4. If it has changed, fall back to per-file blob hashes

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Git-based file state tracker
pub struct GitCacheInvalidator {
    /// Map of file path → git blob hash (from `git ls-files -s`)
    file_hashes: HashMap<PathBuf, String>,
    /// Git tree hash of the root directory (from `git write-tree`)
    root_tree_hash: Option<String>,
    /// Whether git is available in the repo
    available: bool,
}

impl GitCacheInvalidator {
    /// Create a new invalidator by reading git state from the given repo root
    pub fn new(repo_root: &Path) -> Self {
        let mut invalidator = Self {
            file_hashes: HashMap::new(),
            root_tree_hash: None,
            available: false,
        };

        match Self::read_git_state(repo_root) {
            Ok((file_hashes, tree_hash)) => {
                invalidator.file_hashes = file_hashes;
                // `HEAD^{tree}` only describes the committed state. With
                // uncommitted edits to tracked files it would claim "nothing
                // changed" and let stale cached output be reused, so a dirty
                // worktree has no usable tree hash (=> "changed").
                invalidator.root_tree_hash = if Self::worktree_is_dirty(repo_root) {
                    None
                } else {
                    Some(tree_hash)
                };
                invalidator.available = true;
                info!(
                    "Git cache invalidator: {} tracked files, tree hash: {}",
                    invalidator.file_hashes.len(),
                    invalidator.root_tree_hash.as_deref().unwrap_or("?")
                );
            }
            Err(e) => {
                warn!(
                    "Git cache invalidator unavailable ({}), falling back to content hashes",
                    e
                );
            }
        }

        invalidator
    }

    /// Check if git-based invalidation is available
    pub fn is_available(&self) -> bool {
        self.available
    }

    /// Get the root tree hash (captures entire repo state)
    pub fn root_tree_hash(&self) -> Option<&str> {
        self.root_tree_hash.as_deref()
    }

    /// Get the git blob hash for a specific file
    pub fn file_hash(&self, path: &Path) -> Option<&str> {
        self.file_hashes.get(path).map(|s| s.as_str())
    }

    /// Check if a file has changed by comparing git blob hashes
    pub fn has_file_changed(&self, path: &Path, previous_hash: Option<&str>) -> bool {
        match (self.file_hash(path), previous_hash) {
            (Some(current), Some(prev)) => current != prev,
            (Some(_), None) => true, // new file
            (None, Some(_)) => true, // file was removed
            (None, None) => true,    // unknown file
        }
    }

    /// Get all tracked file paths
    pub fn tracked_files(&self) -> Vec<&PathBuf> {
        self.file_hashes.keys().collect()
    }

    /// Compute a composite cache key from the root tree hash.
    /// If the tree hash is the same, the entire build can be cached.
    pub fn composite_cache_key(&self) -> Option<String> {
        self.root_tree_hash.clone()
    }

    /// True when tracked files differ from the index/HEAD (or when git
    /// cannot tell us — treated as dirty to stay on the safe side).
    fn worktree_is_dirty(repo_root: &Path) -> bool {
        match Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=no"])
            .current_dir(repo_root)
            .output()
        {
            Ok(out) if out.status.success() => !out.stdout.is_empty(),
            _ => true,
        }
    }

    /// Read git state: file blob hashes and root tree hash
    fn read_git_state(repo_root: &Path) -> Result<(HashMap<PathBuf, String>, String)> {
        // Get all tracked files with their blob hashes
        // `git ls-files -s` output: "<mode> <hash> <stage>\t<path>"
        let output = Command::new("git")
            .args(["ls-files", "-s"])
            .current_dir(repo_root)
            .output()?;

        if !output.status.success() {
            anyhow::bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut file_hashes = HashMap::new();

        for line in stdout.lines() {
            // Parse: "100644 <hash> 0\t<path>"
            let parts: Vec<&str> = line.splitn(2, '\t').collect();
            if parts.len() != 2 {
                continue;
            }
            let meta: Vec<&str> = parts[0].split_whitespace().collect();
            if meta.len() < 2 {
                continue;
            }
            let blob_hash = meta[1].to_string();
            let file_path = repo_root.join(parts[1]);
            file_hashes.insert(file_path, blob_hash);
        }

        // Get the root tree hash via `git write-tree`
        // This requires the index to be up-to-date. We use `git rev-parse HEAD^{tree}`
        // as a fallback which gets the tree hash of the current commit.
        let tree_hash = Self::get_tree_hash(repo_root)?;

        Ok((file_hashes, tree_hash))
    }

    /// Get the git tree hash for the current HEAD
    fn get_tree_hash(repo_root: &Path) -> Result<String> {
        // Try HEAD^{tree} first (committed state)
        let output = Command::new("git")
            .args(["rev-parse", "HEAD^{tree}"])
            .current_dir(repo_root)
            .output();

        match output {
            Ok(result) if result.status.success() => {
                let hash = String::from_utf8_lossy(&result.stdout).trim().to_string();
                if !hash.is_empty() {
                    return Ok(hash);
                }
            }
            _ => {}
        }

        // Fallback: try writing the current index as a tree
        // This captures staged but uncommitted changes too
        let output = Command::new("git")
            .args(["write-tree"])
            .current_dir(repo_root)
            .output();

        match output {
            Ok(result) if result.status.success() => {
                let hash = String::from_utf8_lossy(&result.stdout).trim().to_string();
                if !hash.is_empty() {
                    return Ok(hash);
                }
            }
            _ => {}
        }

        // Last resort: use a hash of all file blob hashes
        let mut all_hashes: Vec<(String, String)> = file_hashes_iter(repo_root)?
            .into_iter()
            .map(|(p, h)| (p.to_string_lossy().to_string(), h))
            .collect();
        all_hashes.sort_by(|a, b| a.0.cmp(&b.0));

        let combined: String = all_hashes
            .iter()
            .map(|(p, h)| format!("{}:{}", p, h))
            .collect::<Vec<_>>()
            .join("|");

        let hash = blake3::hash(combined.as_bytes());
        Ok(hash.to_hex().as_str().to_string())
    }

    /// Check if the repo state has changed since the last build
    /// by comparing tree hashes
    pub fn has_repo_changed(&self, previous_tree_hash: Option<&str>) -> bool {
        match (self.root_tree_hash(), previous_tree_hash) {
            (Some(current), Some(prev)) => current != prev,
            _ => true, // If we can't determine, assume changed
        }
    }
}

fn file_hashes_iter(repo_root: &Path) -> Result<Vec<(PathBuf, String)>> {
    let output = Command::new("git")
        .args(["ls-files", "-s"])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut result = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(2, '\t').collect();
        if parts.len() != 2 {
            continue;
        }
        let meta: Vec<&str> = parts[0].split_whitespace().collect();
        if meta.len() < 2 {
            continue;
        }
        let blob_hash = meta[1].to_string();
        let file_path = repo_root.join(parts[1]);
        result.push((file_path, blob_hash));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git available");
        assert!(st.status.success(), "git {:?} failed", args);
    }

    #[test]
    fn dirty_worktree_is_reported_as_changed() {
        let dir = std::env::temp_dir().join(format!("pledgepack_git_dirty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.ts"), "export const a = 1;").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        let clean = GitCacheInvalidator::new(&dir);
        let clean_hash = clean.root_tree_hash().map(|s| s.to_string());
        assert!(clean_hash.is_some());
        assert!(!clean.has_repo_changed(clean_hash.as_deref()));

        // Uncommitted edit: HEAD^{tree} is unchanged, but the repo has changed.
        std::fs::write(dir.join("a.ts"), "export const a = 2;").unwrap();
        let dirty = GitCacheInvalidator::new(&dir);
        assert!(
            dirty.has_repo_changed(clean_hash.as_deref()),
            "uncommitted edits must invalidate the tree-hash fast path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_invalid_without_git() {
        // In a non-git directory, the invalidator should be unavailable
        let tmp = std::env::temp_dir().join("pledgepack_git_test_nonexistent");
        let invalidator = GitCacheInvalidator::new(&tmp);
        // May or may not be available depending on test environment
        // Just ensure it doesn't panic
        let _ = invalidator.is_available();
    }
}
