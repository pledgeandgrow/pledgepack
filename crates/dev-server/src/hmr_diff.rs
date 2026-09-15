// HMR partial update support — line-level diff computation
//
// Instead of sending the full module on every change, we compute a line-level
// diff and send only the changed lines. This significantly reduces WebSocket
// bandwidth for large modules with small edits.
//
// Uses the `similar` crate (Myers diff algorithm) for robust, efficient
// diffing with no line-count limits.

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

/// A single diff operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffOp {
    /// Lines were added at the given line number
    #[serde(rename = "insert")]
    Insert { line: u32, content: Vec<String> },
    /// Lines were removed at the given line number
    #[serde(rename = "delete")]
    Delete { line: u32, count: u32 },
    /// Lines were replaced at the given line number
    #[serde(rename = "replace")]
    Replace {
        line: u32,
        count: u32,
        content: Vec<String>,
    },
}

/// A line-level diff between two versions of a module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineDiff {
    pub ops: Vec<DiffOp>,
    /// Total lines in the old version
    pub old_lines: u32,
    /// Total lines in the new version
    pub new_lines: u32,
}

/// Configuration for HMR diff heuristics.
///
/// Controls the thresholds that determine whether a line-level diff is small
/// enough to be sent as a partial update instead of the full module code.
#[derive(Debug, Clone)]
pub struct HmrDiffConfig {
    /// Maximum number of diff operations for a diff to be considered "small".
    /// Diffs with more operations than this will fall back to full code.
    pub small_diff_threshold: usize,
    /// Maximum fraction of changed lines (0.0–1.0) for a diff to be considered "small".
    /// Diffs affecting more than this fraction of the module fall back to full code.
    pub small_diff_percentage: f64,
}

impl Default for HmrDiffConfig {
    fn default() -> Self {
        Self {
            small_diff_threshold: 10,
            small_diff_percentage: 0.3,
        }
    }
}

impl LineDiff {
    /// Check if the diff is small enough to be worth sending instead of full code.
    ///
    /// Uses the provided [`HmrDiffConfig`] thresholds. A diff is "small" when it
    /// has fewer than `config.small_diff_threshold` operations AND affects less
    /// than `config.small_diff_percentage` of the module's lines.
    pub fn is_small(&self, config: &HmrDiffConfig) -> bool {
        if self.ops.len() >= config.small_diff_threshold {
            return false;
        }
        let changed_lines: u32 = self
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Insert { content, .. } => content.len() as u32,
                DiffOp::Delete { count, .. } => *count,
                DiffOp::Replace { count, content, .. } => (*count).max(content.len() as u32),
            })
            .sum();
        let max_lines = self.old_lines.max(self.new_lines).max(1);
        (changed_lines as f64) < (max_lines as f64 * config.small_diff_percentage)
    }

    /// Check if the diff is small using the default thresholds.
    /// Convenience method for callers that don't need custom configuration.
    pub fn is_small_default(&self) -> bool {
        self.is_small(&HmrDiffConfig::default())
    }
}

/// Compute a line-level diff between old and new source code.
/// Uses the `similar` crate's Myers diff algorithm for efficient, correct
/// diffing without the previous 200-line cap.
pub fn compute_diff(old: &str, new: &str) -> LineDiff {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let old_len = old_lines.len() as u32;
    let new_len = new_lines.len() as u32;

    // Fast path: identical
    if old == new {
        return LineDiff {
            ops: Vec::new(),
            old_lines: old_len,
            new_lines: new_len,
        };
    }

    // Fast path: one side empty
    if old_lines.is_empty() {
        return LineDiff {
            ops: vec![DiffOp::Insert {
                line: 0,
                content: new_lines.iter().map(|s| s.to_string()).collect(),
            }],
            old_lines: old_len,
            new_lines: new_len,
        };
    }
    if new_lines.is_empty() {
        return LineDiff {
            ops: vec![DiffOp::Delete {
                line: 0,
                count: old_len,
            }],
            old_lines: old_len,
            new_lines: new_len,
        };
    }

    // Use similar's TextDiff for line-level diffing
    let diff = TextDiff::from_lines(old, new);
    let ops = similar_to_diff_ops(&diff);

    LineDiff {
        ops,
        old_lines: old_len,
        new_lines: new_len,
    }
}

/// Convert similar's diff ops into our DiffOp format, coalescing
/// adjacent inserts and deletes into Insert/Delete/Replace operations.
fn similar_to_diff_ops<'a>(diff: &TextDiff<'a, 'a, 'a, str>) -> Vec<DiffOp> {
    let mut ops = Vec::new();
    let mut old_line: u32 = 0;
    let mut pending_inserts: Vec<String> = Vec::new();
    let mut pending_deletes: u32 = 0;
    let mut pending_delete_start: u32 = 0;
    // Line position where the current pending insert block begins (in old-file
    // coordinates). Tracked separately from `pending_delete_start` so a pure
    // insert doesn't inherit a stale delete position from a previous flush.
    let mut pending_insert_pos: u32 = 0;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                // Flush pending ops at the current position
                flush_pending(
                    &mut ops,
                    &mut pending_inserts,
                    &mut pending_deletes,
                    &mut pending_delete_start,
                    &mut pending_insert_pos,
                );
                old_line += 1;
            }
            ChangeTag::Delete => {
                if pending_deletes == 0 {
                    pending_delete_start = old_line;
                }
                pending_deletes += 1;
                old_line += 1;
            }
            ChangeTag::Insert => {
                if pending_inserts.is_empty() {
                    // First insert of a new pending block — record where it
                    // goes in old-file line coordinates.
                    pending_insert_pos = old_line;
                }
                pending_inserts.push(change.value().trim_end_matches('\n').to_string());
            }
        }
    }

    // Flush any remaining pending ops
    flush_pending(
        &mut ops,
        &mut pending_inserts,
        &mut pending_deletes,
        &mut pending_delete_start,
        &mut pending_insert_pos,
    );

    ops
}

/// Flush pending insert/delete operations into a single DiffOp
fn flush_pending(
    ops: &mut Vec<DiffOp>,
    inserts: &mut Vec<String>,
    deletes: &mut u32,
    delete_start: &mut u32,
    insert_pos: &mut u32,
) {
    if *deletes > 0 && !inserts.is_empty() {
        ops.push(DiffOp::Replace {
            line: *delete_start,
            count: *deletes,
            content: std::mem::take(inserts),
        });
    } else if *deletes > 0 {
        ops.push(DiffOp::Delete {
            line: *delete_start,
            count: *deletes,
        });
    } else if !inserts.is_empty() {
        // Pure insert — use the recorded insert position, not delete_start
        // (which may hold a stale value from a previous flush).
        ops.push(DiffOp::Insert {
            line: *insert_pos,
            content: std::mem::take(inserts),
        });
    }
    *deletes = 0;
}
