// Read tracker — automatic file-read dependency tracking.
//
// PledgePack uses explicit dependencies (Task<T> arguments) as its primary
// dependency tracking mechanism. This is safer and more deterministic than
// Turbopack's read interception. However, there are cases where a task
// reads files that aren't declared as explicit dependencies — for example,
// a transform task might read a `tsconfig.json` or `.browserslistrc` file
// that affects its output but isn't passed as a `Task<T>` argument.
//
// The `ReadTracker` supplements explicit dependencies:
//   1. Before task execution, a `ReadTracker` is installed in a thread-local.
//   2. File I/O functions (or wrappers) call `ReadTracker::record_read()`.
//   3. After task execution, the recorded reads are merged with explicit
//      dependencies and stored in `StoredOutput`.
//   4. When any recorded file changes, the task is invalidated.
//
// This is NOT a replacement for explicit dependencies — it's a safety net
// for implicit dependencies that would otherwise cause stale cache hits.
//
// # Design Trade-offs
//
// - Thread-local, not global: each task execution gets its own tracker.
// - Zero overhead when disabled: `record_read()` checks a thread-local bool.
// - Deterministic: the same task reading the same files always produces the
//   same dependency set. No non-determinism from filesystem state.
// - Opt-in: tasks that don't do file I/O pay no cost.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A tracker for file reads during task execution.
///
/// Installed in a thread-local before task execution and collected after.
/// Records absolute file paths that were read during the task's computation.
#[derive(Debug, Default)]
pub struct ReadTracker {
    /// Set of absolute file paths read during task execution.
    reads: HashSet<PathBuf>,
    /// Whether read tracking is active for this thread.
    active: bool,
}

impl ReadTracker {
    /// Create a new empty read tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a file read.
    pub fn record_read<P: AsRef<Path>>(&mut self, path: P) {
        if self.active {
            let abs = if path.as_ref().is_absolute() {
                path.as_ref().to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_default()
                    .join(path.as_ref())
            };
            // Canonicalize to handle symlinks and relative paths consistently.
            // Falls back to the non-canonical absolute path if canonicalization
            // fails (e.g., the file doesn't exist yet).
            let canonical = std::fs::canonicalize(&abs).unwrap_or(abs);
            self.reads.insert(canonical);
        }
    }

    /// Get all recorded file reads.
    pub fn reads(&self) -> &HashSet<PathBuf> {
        &self.reads
    }

    /// Number of recorded reads.
    pub fn len(&self) -> usize {
        self.reads.len()
    }

    /// Whether any reads were recorded.
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty()
    }

    /// Take ownership of the recorded reads, leaving the tracker empty.
    pub fn take_reads(&mut self) -> HashSet<PathBuf> {
        std::mem::take(&mut self.reads)
    }

    /// Activate read tracking.
    pub fn activate(&mut self) {
        self.active = true;
    }

    /// Deactivate read tracking.
    pub fn deactivate(&mut self) {
        self.active = false;
    }

    /// Whether read tracking is active.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

// --- Thread-Local Read Tracker ---

thread_local! {
    /// The current read tracker for the executing task on this thread.
    /// Set by `TaskEngine::compute_task()` before calling the executor.
    static CURRENT_READ_TRACKER: RefCell<Option<ReadTracker>> = const { RefCell::new(None) };
}

/// Record a file read in the current thread's read tracker.
///
/// This is called by file I/O wrappers. If no task is executing (no tracker
/// is installed), this is a no-op.
pub fn record_read<P: AsRef<Path>>(path: P) {
    CURRENT_READ_TRACKER.with(|tracker| {
        if let Some(tracker) = tracker.borrow_mut().as_mut() {
            tracker.record_read(path);
        }
    });
}

/// Install a read tracker for the duration of a closure.
///
/// Returns the closure's result and the tracker with recorded reads. Only
/// exercised by this module's own tests today — production code runs async
/// task executors, which need the install/collect pair below instead (a
/// closure can't span an `.await`).
#[allow(dead_code)]
pub(crate) fn with_read_tracker<F, R>(f: F) -> (R, ReadTracker)
where
    F: FnOnce() -> R,
{
    install_tracker();
    let result = f();
    let tracker = collect_tracker();
    (result, tracker)
}

/// Install a read tracker on the current thread.
///
/// Must be paired with `collect_tracker()`. Used for async contexts where
/// the closure-based `with_read_tracker()` won't work (the future needs to
/// be awaited between install and collect).
pub fn install_tracker() {
    let mut tracker = ReadTracker::new();
    tracker.activate();
    CURRENT_READ_TRACKER.with(|cell| {
        *cell.borrow_mut() = Some(tracker);
    });
}

/// Collect and remove the read tracker from the current thread.
///
/// Returns the tracker with all recorded file reads. If no tracker was
/// installed, returns an empty tracker.
pub fn collect_tracker() -> ReadTracker {
    CURRENT_READ_TRACKER
        .with(|cell| cell.borrow_mut().take())
        .unwrap_or_default()
}

/// Check if read tracking is active on this thread.
pub fn is_tracking() -> bool {
    CURRENT_READ_TRACKER.with(|tracker| {
        tracker
            .borrow()
            .as_ref()
            .map(|t| t.is_active())
            .unwrap_or(false)
    })
}

/// A wrapper around file reading that records reads in the tracker.
///
/// Use this instead of `std::fs::read_to_string` inside task executors
/// to automatically track file dependencies.
pub fn read_to_string<P: AsRef<Path>>(path: P) -> std::io::Result<String> {
    let path_ref = path.as_ref();
    record_read(path_ref);
    std::fs::read_to_string(path_ref)
}

/// A wrapper around file reading that records reads in the tracker.
pub fn read<P: AsRef<Path>>(path: P) -> std::io::Result<Vec<u8>> {
    let path_ref = path.as_ref();
    record_read(path_ref);
    std::fs::read(path_ref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_tracker_records_reads() {
        let mut tracker = ReadTracker::new();
        tracker.activate();
        tracker.record_read("/foo/bar.ts");
        tracker.record_read("/foo/baz.tsx");
        tracker.record_read("/foo/bar.ts"); // dedup

        let reads = tracker.reads();
        assert_eq!(reads.len(), 2);
        // On Windows, /foo/bar.ts is not absolute, so it gets joined with cwd.
        // Check that the paths end with the expected components.
        let has_bar = reads
            .iter()
            .any(|p| p.to_string_lossy().ends_with("foo/bar.ts"));
        let has_baz = reads
            .iter()
            .any(|p| p.to_string_lossy().ends_with("foo/baz.tsx"));
        assert!(
            has_bar,
            "Expected a path ending with foo/bar.ts, got: {:?}",
            reads
        );
        assert!(
            has_baz,
            "Expected a path ending with foo/baz.tsx, got: {:?}",
            reads
        );
    }

    #[test]
    fn read_tracker_inactive_does_not_record() {
        let mut tracker = ReadTracker::new();
        // Not activated — should not record
        tracker.record_read("/foo/bar.ts");
        assert!(tracker.is_empty());
    }

    #[test]
    fn with_read_tracker_captures_reads() {
        let (result, tracker) = with_read_tracker(|| {
            record_read("/some/file.ts");
            record_read("/other/file.css");
            42
        });

        assert_eq!(result, 42);
        assert_eq!(tracker.len(), 2);
        // On Windows, paths are joined with cwd. Check by suffix.
        let has_some = tracker
            .reads()
            .iter()
            .any(|p| p.to_string_lossy().ends_with("some/file.ts"));
        let has_other = tracker
            .reads()
            .iter()
            .any(|p| p.to_string_lossy().ends_with("other/file.css"));
        assert!(
            has_some,
            "Expected a path ending with some/file.ts, got: {:?}",
            tracker.reads()
        );
        assert!(
            has_other,
            "Expected a path ending with other/file.css, got: {:?}",
            tracker.reads()
        );
    }

    #[test]
    fn with_read_tracker_no_reads() {
        let (result, tracker) = with_read_tracker(|| "hello");

        assert_eq!(result, "hello");
        assert!(tracker.is_empty());
    }

    #[test]
    fn record_read_outside_tracker_is_noop() {
        // No tracker installed — should not panic
        record_read("/nonexistent/path.ts");
    }

    #[test]
    fn read_to_string_tracks_read() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("test.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        writeln!(file, "hello world").unwrap();

        let (_result, tracker) = with_read_tracker(|| read_to_string(&file_path).unwrap());

        // record_read canonicalizes the path, so compare against the
        // canonical version of the file path.
        let canonical = std::fs::canonicalize(&file_path).unwrap_or(file_path);
        assert!(tracker.reads().contains(&canonical));
    }

    #[test]
    fn take_reads_empties_tracker() {
        let mut tracker = ReadTracker::new();
        tracker.activate();
        tracker.record_read("/foo.ts");
        assert_eq!(tracker.len(), 1);

        let reads = tracker.take_reads();
        assert_eq!(reads.len(), 1);
        assert!(tracker.is_empty());
    }

    #[test]
    fn is_tracking_false_outside_tracker() {
        assert!(!is_tracking());
    }

    #[test]
    fn is_tracking_true_inside_tracker() {
        let (_result, _tracker) = with_read_tracker(|| {
            assert!(is_tracking());
        });
    }
}
