// Native file system watcher with platform-specific backends
//
// Uses native OS APIs directly for lower latency than the notify crate abstraction:
//   - Windows: ReadDirectoryChangesW via a dedicated thread per directory
//   - Linux: inotify via libc
//   - macOS: FSEvents via libc
//
// Falls back to the notify crate if native APIs are unavailable.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A file change event from the native watcher
#[derive(Debug, Clone)]
pub struct FileEvent {
    pub path: PathBuf,
    // PRODUCTION-READINESS-100.md goal 91: the struct-level `#[allow(dead_code)]`
    // that used to be here was imprecise — `path` above is read all over
    // `crates/dev-server/src/lib.rs`'s HMR handling, only `kind` is
    // populated (every `FileEvent { path, kind }` construction site sets
    // it) but never actually read by any consumer today. Reserved for
    // distinguishing create/modify/remove for smarter HMR (e.g. skipping
    // work on a delete) — moved the allow down to just this field instead
    // of suppressing the whole struct, so a future genuinely-dead field
    // added here wouldn't go unnoticed under the same broad allow.
    #[allow(dead_code)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Create,
    Modify,
    Remove,
}

/// Combine the pending event kind with a newer event for the same path inside
/// one debounce window: a file that was just created and is then written to
/// is still a `Create`; otherwise the latest event wins.
#[allow(dead_code)] // only the native (per-OS) watcher backends call this
fn merge_event_kind(pending: EventKind, new: EventKind) -> EventKind {
    match (pending, new) {
        (EventKind::Create, EventKind::Modify) => EventKind::Create,
        (_, new) => new,
    }
}

/// Configuration for the file watcher
pub struct WatcherConfig {
    /// Debounce duration — events within this window are coalesced
    pub debounce_ms: u64,
    /// File extensions to watch (empty = all)
    pub extensions: Vec<String>,
    /// Directories to ignore
    pub ignore_dirs: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 100,
            extensions: vec![
                "ts".into(),
                "tsx".into(),
                "js".into(),
                "jsx".into(),
                "css".into(),
                "json".into(),
                "vue".into(),
                "svelte".into(),
                "scss".into(),
                "less".into(),
                "html".into(),
                "astro".into(),
            ],
            ignore_dirs: vec![
                "node_modules".into(),
                ".git".into(),
                "dist".into(),
                ".pledge-cache".into(),
                "target".into(),
            ],
        }
    }
}

/// Start watching a directory tree. Returns a receiver for file events.
///
/// Uses native OS APIs where available, falling back to notify crate.
pub fn start_watcher(root: &Path, config: WatcherConfig) -> mpsc::Receiver<FileEvent> {
    let (tx, rx) = mpsc::channel::<FileEvent>();

    let root = root.to_path_buf();

    std::thread::spawn(move || {
        #[cfg(target_os = "windows")]
        {
            if let Err(e) = watch_windows(&root, &config, &tx) {
                warn!(
                    "Windows native watcher failed: {}, falling back to notify",
                    e
                );
                watch_notify_fallback(&root, &config, &tx);
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Err(e) = watch_linux(&root, &config, &tx) {
                warn!(
                    "Linux inotify watcher failed: {}, falling back to notify",
                    e
                );
                watch_notify_fallback(&root, &config, &tx);
            }
        }

        #[cfg(target_os = "macos")]
        {
            if let Err(e) = watch_macos(&root, &config, &tx) {
                warn!(
                    "macOS FSEvents watcher failed: {}, falling back to notify",
                    e
                );
                watch_notify_fallback(&root, &config, &tx);
            }
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        {
            watch_notify_fallback(&root, &config, &tx);
        }
    });

    rx
}

/// Strip the Windows verbatim prefix (`\\?\C:\..` -> `C:\..`,
/// `\\?\UNC\srv\share` -> `\\srv\share`) from a path string.
fn strip_verbatim_str(s: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{}", rest))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// The part of `path` below the watched `root`. Only these components are
/// subject to the ignore list: a project that merely *lives under* a directory
/// called `target`/`dist`/`node_modules` must still be watched.
fn relative_to_root(path: &Path, root: &Path) -> PathBuf {
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_path_buf();
    }
    // Verbatim (`\\?\`) vs. plain spelling of the same location.
    let p = path.to_string_lossy();
    let r = root.to_string_lossy();
    let (p, r) = (strip_verbatim_str(&p), strip_verbatim_str(&r));
    if let Ok(rel) = Path::new(p.as_ref()).strip_prefix(Path::new(r.as_ref())) {
        return rel.to_path_buf();
    }
    // Unrelated to the root: only the file name can meaningfully be checked.
    path.file_name().map(PathBuf::from).unwrap_or_default()
}

/// Whether the directory `dir` should be skipped when walking `root`
/// (the root itself is never ignored, whatever it is called).
#[allow(dead_code)] // only the inotify backend and the rescan walk directories
fn is_ignored_dir(dir: &Path, root: &Path, config: &WatcherConfig) -> bool {
    relative_to_root(dir, root).components().any(|c| {
        config
            .ignore_dirs
            .contains(&c.as_os_str().to_string_lossy().to_string())
    })
}

/// Check if a path should be watched based on config. Ignore-list matching is
/// done on the components *relative to `root`* only.
fn should_watch(path: &Path, root: &Path, config: &WatcherConfig) -> bool {
    let rel = relative_to_root(path, root);
    for component in rel.components() {
        let name = component.as_os_str().to_string_lossy().to_string();
        if config.ignore_dirs.contains(&name) {
            return false;
        }
    }

    // Check extension filter
    if config.extensions.is_empty() {
        return true;
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    config.extensions.iter().any(|e| e == ext)
}

/// Trailing-edge per-path debouncer: an event is released once its path has
/// been quiet for `dur`. Unlike a single-slot debouncer it never drops or
/// reorders events for different paths that interleave inside the window.
#[allow(dead_code)] // only the Windows backend uses it (and the unit tests)
struct Debouncer {
    dur: Duration,
    pending: Vec<(PathBuf, EventKind, Instant)>,
}

#[allow(dead_code)]
impl Debouncer {
    fn new(dur: Duration) -> Self {
        Self {
            dur,
            pending: Vec::new(),
        }
    }

    fn push(&mut self, path: PathBuf, kind: EventKind, now: Instant) {
        if let Some(slot) = self.pending.iter_mut().find(|(p, _, _)| *p == path) {
            slot.1 = merge_event_kind(slot.1, kind);
            slot.2 = now;
        } else {
            self.pending.push((path, kind, now));
        }
    }

    /// Remove and return every event whose quiet period has elapsed.
    fn drain_ready(&mut self, now: Instant) -> Vec<FileEvent> {
        let dur = self.dur;
        let mut ready = Vec::new();
        self.pending.retain(|(path, kind, last)| {
            if now.duration_since(*last) >= dur {
                ready.push(FileEvent {
                    path: path.clone(),
                    kind: *kind,
                });
                false
            } else {
                true
            }
        });
        ready
    }

    /// How long until the next pending event becomes ready (None = nothing pending).
    fn next_wait(&self, now: Instant) -> Option<Duration> {
        self.pending
            .iter()
            .map(|(_, _, last)| (*last + self.dur).saturating_duration_since(now))
            .min()
    }
}

/// Walk `root` and return every watched file modified at or after `since`.
/// Used to recover after the OS event buffer overflowed.
#[allow(dead_code)]
fn rescan_modified_since(
    root: &Path,
    config: &WatcherConfig,
    since: std::time::SystemTime,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if !is_ignored_dir(&path, root, config) {
                    stack.push(path);
                }
            } else if should_watch(&path, root, config)
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|m| m >= since)
                    .unwrap_or(true)
            {
                out.push(path);
            }
        }
    }
    out
}

// ─── Windows: ReadDirectoryChangesW ──────────────────────────────────────────

#[cfg(target_os = "windows")]
fn watch_windows(
    root: &Path,
    config: &WatcherConfig,
    tx: &mpsc::Sender<FileEvent>,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_INCOMPLETE, ERROR_NOTIFY_ENUM_DIR, GetLastError, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
        FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

    /// 64 KiB. Held as `u32`s so the buffer is DWORD-aligned as
    /// ReadDirectoryChangesW requires.
    const BUFFER_WORDS: usize = 16 * 1024;

    /// Owns the directory handle, the completion event and the buffers. The
    /// buffers live in a `Box` so their addresses stay stable while a request
    /// is pending; `Drop` cancels an in-flight request and waits for the
    /// kernel to finish with the buffer *before* freeing it.
    struct DirWatch {
        handle: HANDLE,
        event: HANDLE,
        overlapped: Box<OVERLAPPED>,
        buffer: Box<[u32]>,
        armed: bool,
    }

    impl DirWatch {
        /// Issue a new ReadDirectoryChangesW. Must only be called when no
        /// request is pending (the OVERLAPPED/buffer are in use until it
        /// completes).
        fn arm(&mut self) -> Result<(), String> {
            debug_assert!(!self.armed);
            unsafe { ResetEvent(self.event) };
            *self.overlapped = unsafe { std::mem::zeroed() };
            self.overlapped.hEvent = self.event;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    self.handle,
                    self.buffer.as_mut_ptr() as *mut _,
                    (self.buffer.len() * 4) as u32,
                    1, // bWatchSubtree = TRUE
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE
                        | FILE_NOTIFY_CHANGE_CREATION,
                    ptr::null_mut(),
                    &mut *self.overlapped,
                    None,
                )
            };
            if ok == 0 {
                return Err(format!("ReadDirectoryChangesW failed ({})", unsafe {
                    GetLastError()
                }));
            }
            self.armed = true;
            Ok(())
        }
    }

    impl Drop for DirWatch {
        fn drop(&mut self) {
            unsafe {
                if self.armed {
                    CancelIoEx(self.handle, &*self.overlapped);
                    let mut n = 0u32;
                    // Block until the kernel is done with buffer/overlapped.
                    GetOverlappedResult(self.handle, &*self.overlapped, &mut n, 1);
                }
                CloseHandle(self.handle);
                CloseHandle(self.event);
            }
        }
    }

    // Convert path to wide string
    let wide_path: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!("CreateFileW failed ({})", unsafe {
            GetLastError()
        }));
    }

    // Manual-reset event, initially non-signalled.
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        unsafe { CloseHandle(handle) };
        return Err("CreateEventW failed".into());
    }

    let mut watch = DirWatch {
        handle,
        event,
        overlapped: Box::new(unsafe { std::mem::zeroed() }),
        buffer: vec![0u32; BUFFER_WORDS].into_boxed_slice(),
        armed: false,
    };
    watch.arm()?;

    let mut debouncer = Debouncer::new(Duration::from_millis(config.debounce_ms));
    let mut last_activity = std::time::SystemTime::now();

    info!("Native Windows file watcher started on {}", pledgepack_core::display_path(&root));

    loop {
        // Sleep until either the kernel completes the (single) pending
        // request or the oldest debounced event is due.
        let wait_ms = match debouncer.next_wait(Instant::now()) {
            Some(d) => (d.as_millis() as u32).max(1),
            None => 1000,
        };
        let wait = unsafe { WaitForSingleObject(watch.event, wait_ms) };
        if wait == WAIT_FAILED {
            return Err("WaitForSingleObject failed".into());
        }

        if wait == WAIT_OBJECT_0 {
            // The request completed: collect its result. It is only re-armed
            // after the data is copied out, and never while still pending.
            let mut bytes: u32 = 0;
            let ok =
                unsafe { GetOverlappedResult(watch.handle, &*watch.overlapped, &mut bytes, 0) };
            let err = if ok == 0 {
                unsafe { GetLastError() }
            } else {
                0
            };
            if ok == 0 && err == ERROR_IO_INCOMPLETE {
                // Spurious wake-up: still pending, do NOT re-arm.
                continue;
            }
            watch.armed = false;

            // Copy the records out so the request can be re-issued right away
            // (changes made while no request is pending would be lost).
            let data: Vec<u8> = {
                let view: &[u8] = unsafe {
                    std::slice::from_raw_parts(
                        watch.buffer.as_ptr() as *const u8,
                        watch.buffer.len() * 4,
                    )
                };
                view[..(bytes as usize).min(view.len())].to_vec()
            };
            watch.arm()?;

            let overflowed = (ok != 0 && bytes == 0) || err == ERROR_NOTIFY_ENUM_DIR;
            let now = Instant::now();
            if overflowed {
                // The kernel buffer overflowed: individual events were lost.
                // Rescan for anything modified since the last activity.
                warn!(
                    "file watcher buffer overflow - rescanning {}",
                    pledgepack_core::display_path(&root)
                );
                let since = last_activity - Duration::from_secs(1);
                for path in rescan_modified_since(root, config, since) {
                    debouncer.push(path, EventKind::Modify, now);
                }
            } else if ok == 0 {
                return Err(format!("GetOverlappedResult failed ({})", err));
            } else {
                for (action, name) in parse_notify_records(&data) {
                    let full_path = root.join(&name);
                    let kind = match action {
                        FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => EventKind::Create,
                        FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => EventKind::Remove,
                        _ => EventKind::Modify,
                    };
                    if should_watch(&full_path, root, config) {
                        debouncer.push(full_path, kind, now);
                    }
                }
            }
            last_activity = std::time::SystemTime::now();
        }

        for ev in debouncer.drain_ready(Instant::now()) {
            if tx.send(ev).is_err() {
                // Receiver dropped: shut down (DirWatch::drop cancels the I/O).
                return Ok(());
            }
        }
    }
}

/// Parse the `FILE_NOTIFY_INFORMATION` records in `data` into
/// `(action, relative path)` pairs. Malformed/truncated records end the walk.
#[allow(dead_code)] // Windows backend + unit tests
fn parse_notify_records(data: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 12 <= data.len() {
        let record = &data[offset..];
        let next = u32::from_le_bytes([record[0], record[1], record[2], record[3]]) as usize;
        let action = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);
        let name_len = u32::from_le_bytes([record[8], record[9], record[10], record[11]]) as usize;
        if record.len() < 12 + name_len {
            break;
        }
        let units: Vec<u16> = record[12..12 + name_len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        out.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        offset += next;
    }
    out
}

// ─── Linux: inotify ──────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn watch_linux(
    root: &Path,
    config: &WatcherConfig,
    tx: &mpsc::Sender<FileEvent>,
) -> Result<(), String> {
    use std::os::unix::io::RawFd;

    const IN_CREATE: u32 = 0x100;
    const IN_DELETE: u32 = 0x200;
    const IN_MOVED_TO: u32 = 0x80;
    const IN_Q_OVERFLOW: u32 = 0x4000;

    // inotify_init1
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err("inotify_init1 failed".into());
    }

    // Recursively add watches
    let mut watch_map: std::collections::HashMap<RawFd, PathBuf> = std::collections::HashMap::new();
    add_watch_recursive(fd, root, root, &mut watch_map, config);

    if watch_map.is_empty() {
        unsafe { libc::close(fd) };
        return Err("No directories to watch".into());
    }

    info!(
        "Native Linux inotify watcher started on {} ({} dirs)",
        pledgepack_core::display_path(&root),
        watch_map.len()
    );

    let mut buffer = vec![0u8; 4096];
    let mut debounce_path: Option<PathBuf> = None;
    let mut debounce_time: Option<Instant> = None;
    // Kind of the pending event: flushing used to hard-code `Modify`, and
    // sending the previous event used the NEW event's kind (so a delete of
    // file B was reported as a delete of the previously pending file A).
    let mut debounce_kind = EventKind::Modify;
    let debounce_dur = Duration::from_millis(config.debounce_ms);

    // Use poll with timeout for debounce checking
    let pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };

    loop {
        let mut pfd = pollfd;
        let timeout_ms = debounce_dur.as_millis() as i32;
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };

        if ret < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            warn!("inotify poll error");
            break;
        }

        if ret == 0 {
            // Timeout — flush debounced event
            if let (Some(path), Some(time)) = (&debounce_path, debounce_time)
                && time.elapsed() > debounce_dur
            {
                if should_watch(path, root, config) {
                    let _ = tx.send(FileEvent {
                        path: path.clone(),
                        kind: debounce_kind,
                    });
                }
                debounce_path = None;
                debounce_time = None;
            }
            continue;
        }

        // Read inotify events
        let len = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut _, buffer.len()) };

        if len <= 0 {
            continue;
        }

        let mut offset = 0usize;
        let len = len as usize;

        while offset + 16 <= len {
            let wd = i32::from_le_bytes([
                buffer[offset],
                buffer[offset + 1],
                buffer[offset + 2],
                buffer[offset + 3],
            ]);
            let mask = u32::from_le_bytes([
                buffer[offset + 4],
                buffer[offset + 5],
                buffer[offset + 6],
                buffer[offset + 7],
            ]);
            let _cookie = u32::from_le_bytes([
                buffer[offset + 8],
                buffer[offset + 9],
                buffer[offset + 10],
                buffer[offset + 11],
            ]);
            let name_len = u32::from_le_bytes([
                buffer[offset + 12],
                buffer[offset + 13],
                buffer[offset + 14],
                buffer[offset + 15],
            ]) as usize;

            offset += 16;

            let dir_path = watch_map.get(&wd).cloned();

            if mask & IN_Q_OVERFLOW != 0 {
                warn!("inotify event queue overflow — some changes may have been missed");
                // Re-add watches to recover
                add_watch_recursive(fd, root, root, &mut watch_map, config);
            }

            if let Some(dir) = &dir_path {
                let full_path = if name_len > 0 && offset + name_len <= len {
                    let name = String::from_utf8_lossy(&buffer[offset..offset + name_len]);
                    let name = name.trim_end_matches('\0');
                    dir.join(name)
                } else {
                    dir.clone()
                };

                let kind = if mask & (IN_CREATE | IN_MOVED_TO) != 0 {
                    EventKind::Create
                } else if mask & IN_DELETE != 0 {
                    EventKind::Remove
                } else {
                    EventKind::Modify
                };

                if should_watch(&full_path, root, config) {
                    let now = Instant::now();
                    let mut merged = kind;
                    if let (Some(prev_path), Some(prev_time)) = (&debounce_path, debounce_time) {
                        if prev_path != &full_path || now.duration_since(prev_time) > debounce_dur {
                            let _ = tx.send(FileEvent {
                                path: prev_path.clone(),
                                kind: debounce_kind,
                            });
                        } else {
                            merged = merge_event_kind(debounce_kind, kind);
                        }
                    }
                    debounce_path = Some(full_path.clone());
                    debounce_time = Some(now);
                    debounce_kind = merged;
                }

                // If a new directory was created, add a watch for it
                if kind == EventKind::Create && full_path.is_dir() {
                    add_watch_recursive(fd, root, &full_path, &mut watch_map, config);
                }
            }

            // Advance to next event (name is null-padded to align to struct size)
            offset += name_len;
            // Align to 16-byte boundary
            offset = (offset + 15) & !15;
        }

        // Flush debounced event if enough time has passed
        if let (Some(path), Some(time)) = (&debounce_path, debounce_time)
            && time.elapsed() > debounce_dur
        {
            if should_watch(path, root, config) {
                let _ = tx.send(FileEvent {
                    path: path.clone(),
                    kind: debounce_kind,
                });
            }
            debounce_path = None;
            debounce_time = None;
        }
    }

    unsafe { libc::close(fd) };
    Ok(())
}

#[cfg(target_os = "linux")]
fn add_watch_recursive(
    fd: std::os::unix::io::RawFd,
    root: &Path,
    dir: &Path,
    watch_map: &mut std::collections::HashMap<std::os::unix::io::RawFd, PathBuf>,
    config: &WatcherConfig,
) {
    // Ignore list applies to components below the watched root only.
    if is_ignored_dir(dir, root, config) {
        return;
    }

    let dir_str = dir.to_string_lossy();
    let c_dir = std::ffi::CString::new(dir_str.as_ref()).unwrap_or_default();
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            c_dir.as_ptr(),
            libc::IN_MODIFY
                | libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_TO
                | libc::IN_MOVED_FROM,
        )
    };

    if wd >= 0 {
        watch_map.insert(wd, dir.to_path_buf());
    }

    // Recurse into subdirectories
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                add_watch_recursive(fd, root, &path, watch_map, config);
            }
        }
    }
}

// ─── macOS: FSEvents ─────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn watch_macos(
    root: &Path,
    config: &WatcherConfig,
    tx: &mpsc::Sender<FileEvent>,
) -> Result<(), String> {
    // FSEvents API is complex and requires CoreFoundation
    // For now, use the notify crate with FSEvents backend directly
    // This is still more efficient than recommended_watcher since we control the event loop
    use notify::{EventKind as NotifyEventKind, RecursiveMode, Watcher};

    let (notify_tx, notify_rx) = mpsc::channel::<notify::Result<notify::Event>>();

    // Use FsEventWatcher directly (macOS native)
    let mut watcher = notify::FsEventWatcher::new(notify_tx, notify::Config::default())
        .map_err(|e| format!("FsEventWatcher creation failed: {}", e))?;

    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| format!("watch failed: {}", e))?;

    // Configure FSEvents for lower latency
    // (notify crate exposes this via configuration)

    info!(
        "Native macOS FSEvents watcher started on {}",
        pledgepack_core::display_path(&root)
    );

    let mut debounce_path: Option<PathBuf> = None;
    let mut debounce_time: Option<Instant> = None;
    // Kind of the pending event: flushing used to hard-code `Modify`, and
    // sending the previous event used the NEW event's kind (so a delete of
    // file B was reported as a delete of the previously pending file A).
    let mut debounce_kind = EventKind::Modify;
    let debounce_dur = Duration::from_millis(config.debounce_ms);

    loop {
        match notify_rx.recv_timeout(debounce_dur) {
            Ok(Ok(event)) => {
                if let NotifyEventKind::Modify(_) | NotifyEventKind::Create(_) = event.kind {
                    for path in &event.paths {
                        if should_watch(path, root, config) {
                            let now = Instant::now();
                            if let (Some(prev_path), Some(prev_time)) =
                                (&debounce_path, debounce_time)
                                && (prev_path != path
                                    || now.duration_since(prev_time) > debounce_dur)
                            {
                                let _ = tx.send(FileEvent {
                                    path: prev_path.clone(),
                                    kind: debounce_kind,
                                });
                            }
                            debounce_path = Some(path.clone());
                            debounce_time = Some(now);
                            debounce_kind = if matches!(event.kind, NotifyEventKind::Create(_)) {
                                EventKind::Create
                            } else {
                                EventKind::Modify
                            };
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                warn!("FSEvents watcher error: {}", e);
            }
            Err(_) => {
                // Timeout — flush debounced event
                if let (Some(path), Some(time)) = (&debounce_path, debounce_time)
                    && time.elapsed() > debounce_dur
                {
                    if should_watch(path, root, config) {
                        let _ = tx.send(FileEvent {
                            path: path.clone(),
                            kind: debounce_kind,
                        });
                    }
                    debounce_path = None;
                    debounce_time = None;
                }
            }
        }
    }
}

// ─── Fallback: notify-debouncer crate ──────────────────────────────────────────────────

// PRODUCTION-READINESS-100.md goal 91: this `#[allow(dead_code)]` was stale —
// `watch_notify_fallback` has 4 real, unconditionally-reachable call sites,
// one in each platform-specific `cfg` branch of `start_watcher()` below.
fn watch_notify_fallback(root: &Path, config: &WatcherConfig, tx: &mpsc::Sender<FileEvent>) {
    use notify::RecursiveMode;
    use notify_debouncer_full::new_debouncer;

    let debounce_dur = Duration::from_millis(config.debounce_ms);

    // Create a debounced watcher that coalesces events within the debounce window
    let (debounce_tx, debounce_rx) = mpsc::channel::<Vec<notify_debouncer_full::DebouncedEvent>>();

    let mut debouncer = match new_debouncer(
        debounce_dur,
        None,
        move |result: Result<Vec<notify_debouncer_full::DebouncedEvent>, Vec<notify::Error>>| {
            if let Ok(events) = result {
                let _ = debounce_tx.send(events);
            }
        },
    ) {
        Ok(d) => d,
        Err(e) => {
            warn!("notify-debouncer creation failed: {}", e);
            return;
        }
    };

    if let Err(e) = debouncer.watch(root, RecursiveMode::Recursive) {
        warn!("notify-debouncer watch failed: {}", e);
        return;
    }

    info!(
        "File watcher started (notify-debouncer fallback) on {}",
        pledgepack_core::display_path(&root)
    );

    loop {
        match debounce_rx.recv_timeout(debounce_dur * 2) {
            Ok(events) => {
                // notify-debouncer coalesces events; emit a FileEvent for each relevant path
                for event in &events {
                    for path in &event.paths {
                        if should_watch(path, root, config) {
                            let kind = match event.kind {
                                notify::EventKind::Create(_) => EventKind::Create,
                                notify::EventKind::Modify(_) => EventKind::Modify,
                                notify::EventKind::Remove(_) => EventKind::Remove,
                                _ => EventKind::Modify,
                            };
                            let _ = tx.send(FileEvent {
                                path: path.clone(),
                                kind,
                            });
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No events within the timeout window — continue waiting
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                warn!("notify-debouncer channel disconnected");
                break;
            }
        }
    }
}

#[cfg(test)]
mod merge_kind_tests {
    use super::*;

    #[test]
    fn create_then_modify_stays_create_and_delete_wins() {
        assert_eq!(
            merge_event_kind(EventKind::Create, EventKind::Modify),
            EventKind::Create
        );
        assert_eq!(
            merge_event_kind(EventKind::Modify, EventKind::Remove),
            EventKind::Remove
        );
        assert_eq!(
            merge_event_kind(EventKind::Remove, EventKind::Create),
            EventKind::Create
        );
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    const WAIT: Duration = Duration::from_secs(6);

    fn cfg() -> WatcherConfig {
        WatcherConfig {
            debounce_ms: 50,
            ..WatcherConfig::default()
        }
    }

    /// Start a watcher and give the backend thread time to arm.
    fn start(root: &Path) -> mpsc::Receiver<FileEvent> {
        let rx = start_watcher(root, cfg());
        std::thread::sleep(Duration::from_millis(500));
        rx
    }

    /// Wait until an event satisfying `pred` arrives.
    fn wait_for(
        rx: &mpsc::Receiver<FileEvent>,
        pred: impl Fn(&FileEvent) -> bool,
    ) -> Option<FileEvent> {
        let deadline = Instant::now() + WAIT;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        a == b || fs::canonicalize(a).ok() == fs::canonicalize(b).ok()
    }

    fn is_for(p: &Path) -> impl Fn(&FileEvent) -> bool + '_ {
        move |e| same_file(&e.path, p)
    }

    #[test]
    fn modify_existing_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("index.tsx");
        fs::write(&file, "a").unwrap();
        let rx = start(dir.path());
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"b").unwrap();
        drop(f);
        assert!(wait_for(&rx, is_for(&file)).is_some(), "no modify event");
    }

    #[test]
    fn create_and_delete_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let rx = start(dir.path());
        let file = dir.path().join("new.ts");
        fs::write(&file, "x").unwrap();
        let ev = wait_for(&rx, is_for(&file)).expect("no create event");
        assert!(matches!(ev.kind, EventKind::Create | EventKind::Modify));
        std::thread::sleep(Duration::from_millis(300));
        fs::remove_file(&file).unwrap();
        let ev = wait_for(&rx, |e| is_for(&file)(e) && e.kind == EventKind::Remove)
            .expect("no remove event");
        assert_eq!(ev.kind, EventKind::Remove);
    }

    #[test]
    fn rename_reports_both_names() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.ts");
        let new = dir.path().join("new.ts");
        fs::write(&old, "x").unwrap();
        let rx = start(dir.path());
        fs::rename(&old, &new).unwrap();
        // The old name is reported as removed, the new one as created.
        let (mut removed_old, mut created_new) = (false, false);
        let deadline = Instant::now() + WAIT;
        while !(removed_old && created_new) {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match rx.recv_timeout(left) {
                Ok(e) if e.path == old && e.kind == EventKind::Remove => removed_old = true,
                Ok(e) if e.path == new && e.kind == EventKind::Create => created_new = true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(removed_old, "old name not reported as removed");
        assert!(created_new, "new name not reported as created");
    }

    #[test]
    fn subdirectory_changes_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        let file = dir.path().join("src/deep/mod.ts");
        fs::write(&file, "1").unwrap();
        let rx = start(dir.path());
        fs::write(&file, "2").unwrap();
        assert!(wait_for(&rx, is_for(&file)).is_some());
    }

    #[test]
    fn rapid_edits_of_many_files_all_arrive() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<PathBuf> = (0..8)
            .map(|i| dir.path().join(format!("f{i}.ts")))
            .collect();
        for f in &files {
            fs::write(f, "0").unwrap();
        }
        let rx = start(dir.path());
        // Interleaved, rapid writes to several files (defeats a single-slot debounce).
        for round in 0..5 {
            for f in &files {
                fs::write(f, round.to_string()).unwrap();
            }
        }
        let mut seen = std::collections::HashSet::new();
        let deadline = Instant::now() + WAIT;
        while seen.len() < files.len() {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match rx.recv_timeout(left) {
                Ok(ev) => {
                    seen.insert(ev.path);
                }
                Err(_) => break,
            }
        }
        assert_eq!(seen.len(), files.len(), "missing events: got {seen:?}");
    }

    #[test]
    fn edits_after_a_quiet_period_keep_flowing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.tsx");
        fs::write(&file, "0").unwrap();
        let rx = start(dir.path());
        for i in 1..=4 {
            fs::write(&file, i.to_string()).unwrap();
            assert!(
                wait_for(&rx, is_for(&file)).is_some(),
                "edit #{i} produced no event"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    #[test]
    fn project_below_an_ignored_named_directory_still_emits_events() {
        for name in ["node_modules", "target", "dist", ".git", ".pledge-cache"] {
            let base = tempfile::tempdir().unwrap();
            let root = base.path().join(name).join("proj");
            fs::create_dir_all(&root).unwrap();
            let file = root.join("index.tsx");
            fs::write(&file, "a").unwrap();
            let rx = start(&root);
            fs::write(&file, "b").unwrap();
            assert!(
                wait_for(&rx, is_for(&file)).is_some(),
                "no event for project under `{name}/`"
            );
        }
    }

    #[test]
    fn ignored_directories_inside_the_root_stay_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        let ignored = dir.path().join("node_modules/pkg/index.js");
        let watched = dir.path().join("ok.ts");
        fs::write(&ignored, "a").unwrap();
        let rx = start(dir.path());
        fs::write(&ignored, "b").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        fs::write(&watched, "b").unwrap();
        let mut got_ignored = false;
        let mut got_watched = false;
        let deadline = Instant::now() + WAIT;
        while !got_watched {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match rx.recv_timeout(left) {
                Ok(ev) if same_file(&ev.path, &ignored) => got_ignored = true,
                Ok(ev) if same_file(&ev.path, &watched) => got_watched = true,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        assert!(got_watched);
        assert!(!got_ignored, "node_modules event leaked through");
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefixed_root_works() {
        let dir = tempfile::tempdir().unwrap();
        // std::fs::canonicalize yields a `\\?\C:\...` path on Windows.
        let root = fs::canonicalize(dir.path()).unwrap();
        assert!(root.to_string_lossy().starts_with(r"\\?\"));
        let file = root.join("v.ts");
        fs::write(&file, "a").unwrap();
        let rx = start(&root);
        fs::write(&file, "b").unwrap();
        let ev = wait_for(&rx, is_for(&file)).expect("no event under verbatim root");
        assert!(ev.path.starts_with(&root));
    }

    #[test]
    fn burst_larger_than_the_os_buffer_is_recovered() {
        // Hundreds of long-named files in one burst can overflow the kernel
        // change buffer; the watcher must not silently drop them.
        let dir = tempfile::tempdir().unwrap();
        let rx = start(dir.path());
        let long = "x".repeat(120);
        let n = 1500;
        for i in 0..n {
            fs::write(dir.path().join(format!("{long}{i}.ts")), "a").unwrap();
        }
        let mut seen = std::collections::HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while seen.len() < n {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match rx.recv_timeout(left) {
                Ok(ev) => {
                    seen.insert(ev.path);
                }
                Err(_) => break,
            }
        }
        assert_eq!(seen.len(), n, "lost events under burst");
    }

    // ---- pure-logic unit tests --------------------------------------------

    #[test]
    fn should_watch_only_checks_components_below_root() {
        let c = WatcherConfig::default();
        let root = Path::new("/home/u/target/dist/proj");
        assert!(should_watch(&root.join("src/a.ts"), root, &c));
        assert!(!should_watch(&root.join("node_modules/x/a.js"), root, &c));
        assert!(!should_watch(&root.join("src/dist/a.js"), root, &c));
        assert!(!should_watch(&root.join("src/a.png"), root, &c));
    }

    #[test]
    fn strip_verbatim_handles_drive_and_unc() {
        assert_eq!(strip_verbatim_str(r"\\?\C:\a"), r"C:\a");
        assert_eq!(strip_verbatim_str(r"\\?\UNC\srv\sh"), r"\\srv\sh");
        assert_eq!(strip_verbatim_str(r"C:\a"), r"C:\a");
    }

    #[test]
    fn is_ignored_dir_never_ignores_the_root_itself() {
        let c = WatcherConfig::default();
        let root = Path::new("/x/node_modules");
        assert!(!is_ignored_dir(root, root, &c));
        assert!(is_ignored_dir(&root.join("a/.git"), root, &c));
    }

    #[test]
    fn debouncer_coalesces_per_path_without_dropping_others() {
        let mut d = Debouncer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        d.push("a".into(), EventKind::Create, t0);
        d.push("b".into(), EventKind::Modify, t0 + Duration::from_millis(10));
        d.push("a".into(), EventKind::Modify, t0 + Duration::from_millis(20));
        assert!(d.drain_ready(t0 + Duration::from_millis(50)).is_empty());
        let out = d.drain_ready(t0 + Duration::from_millis(115));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, PathBuf::from("b"));
        let out = d.drain_ready(t0 + Duration::from_millis(125));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, EventKind::Create);
        assert!(d.next_wait(t0).is_none());
    }

    #[test]
    fn parse_notify_records_decodes_utf16_names() {
        fn rec(next: u32, action: u32, name: &str) -> Vec<u8> {
            let units: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            let mut v = Vec::new();
            v.extend(next.to_le_bytes());
            v.extend(action.to_le_bytes());
            v.extend((units.len() as u32).to_le_bytes());
            v.extend(units);
            v
        }
        let mut first = rec(0, 3, "src\\a\u{e9}.ts");
        let pad = (4 - first.len() % 4) % 4;
        first.extend(std::iter::repeat_n(0u8, pad));
        let next = first.len() as u32;
        first[0..4].copy_from_slice(&next.to_le_bytes());
        first.extend(rec(0, 2, "b.ts"));
        let parsed = parse_notify_records(&first);
        assert_eq!(
            parsed,
            vec![(3, "src\\a\u{e9}.ts".to_string()), (2, "b.ts".to_string())]
        );
        // Truncated input must not panic or loop.
        assert!(parse_notify_records(&first[..10]).is_empty());
    }
}
