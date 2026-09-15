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

/// Check if a path should be watched based on config
fn should_watch(path: &Path, config: &WatcherConfig) -> bool {
    // Check if any path component is in the ignore list
    for component in path.components() {
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

// ─── Windows: ReadDirectoryChangesW ──────────────────────────────────────────

#[cfg(target_os = "windows")]
fn watch_windows(
    root: &Path,
    config: &WatcherConfig,
    tx: &mpsc::Sender<FileEvent>,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
        FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED,
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

    const BUFFER_SIZE: usize = 4096;

    // Convert path to wide string
    let wide_path: Vec<u16> = root
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            0x1, // FILE_LIST_DIRECTORY
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err("CreateFileW failed".into());
    }

    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        unsafe { CloseHandle(handle) };
        return Err("CreateEventW failed".into());
    }

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;

    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut debounce_path: Option<PathBuf> = None;
    let mut debounce_time: Option<Instant> = None;
    let debounce_dur = Duration::from_millis(config.debounce_ms);

    info!("Native Windows file watcher started on {}", root.display());

    loop {
        let mut bytes_returned: u32 = 0;

        let result = unsafe {
            ReadDirectoryChangesW(
                handle,
                buffer.as_mut_ptr() as *mut _,
                BUFFER_SIZE as u32,
                1, // bWatchSubtree = TRUE
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_CREATION,
                &mut bytes_returned,
                &mut overlapped,
                None,
            )
        };

        if result == 0 {
            warn!("ReadDirectoryChangesW failed");
            break;
        }

        // Wait for the overlapped operation to complete
        let wait_result = unsafe {
            WaitForMultipleObjects(
                1,
                [event].as_ptr(),
                0,
                winapi_timeout_ms(config.debounce_ms),
            )
        };

        if wait_result == 0xFFFFFFFF {
            // WAIT_FAILED
            warn!("WaitForMultipleObjects failed");
            break;
        }

        let mut bytes_transferred: u32 = 0;
        let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut bytes_transferred, 0) };
        if ok == 0 || bytes_transferred == 0 {
            // Timeout or no data — check debounce
            if let (Some(path), Some(time)) = (&debounce_path, debounce_time)
                && time.elapsed() > debounce_dur
            {
                if should_watch(path, config) {
                    let _ = tx.send(FileEvent {
                        path: path.clone(),
                        kind: EventKind::Modify,
                    });
                }
                debounce_path = None;
                debounce_time = None;
            }
            continue;
        }

        // Parse the FILE_NOTIFY_INFORMATION records
        let mut offset = 0usize;
        while offset < bytes_transferred as usize {
            let record = &buffer[offset..];
            if record.len() < 12 {
                break;
            }

            let next_offset =
                u32::from_le_bytes([record[0], record[1], record[2], record[3]]) as usize;

            let action = u32::from_le_bytes([record[4], record[5], record[6], record[7]]);

            let name_length =
                u32::from_le_bytes([record[8], record[9], record[10], record[11]]) as usize;

            if record.len() < 12 + name_length {
                break;
            }

            let name_bytes = &record[12..12 + name_length];
            let name_str = String::from_utf16_lossy(bytemuck::cast_slice(name_bytes));

            let full_path = root.join(&name_str);

            let kind = match action {
                FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => EventKind::Create,
                FILE_ACTION_MODIFIED => EventKind::Modify,
                FILE_ACTION_REMOVED => EventKind::Remove,
                _ => EventKind::Modify,
            };

            if should_watch(&full_path, config) {
                // Debounce: coalesce rapid events
                let now = Instant::now();
                if let (Some(prev_path), Some(prev_time)) = (&debounce_path, debounce_time) {
                    if prev_path == &full_path && now.duration_since(prev_time) < debounce_dur {
                        // Still within debounce window, wait more
                    } else {
                        // Previous event is ready to send
                        let _ = tx.send(FileEvent {
                            path: prev_path.clone(),
                            kind,
                        });
                        debounce_path = Some(full_path.clone());
                        debounce_time = Some(now);
                    }
                } else {
                    debounce_path = Some(full_path.clone());
                    debounce_time = Some(now);
                }
            }

            if next_offset == 0 {
                break;
            }
            offset += next_offset;
        }

        // Flush any pending debounced event after processing
        if let (Some(path), Some(time)) = (&debounce_path, debounce_time)
            && time.elapsed() > debounce_dur
        {
            if should_watch(path, config) {
                let _ = tx.send(FileEvent {
                    path: path.clone(),
                    kind: EventKind::Modify,
                });
            }
            debounce_path = None;
            debounce_time = None;
        }
    }

    unsafe {
        CloseHandle(handle);
        CloseHandle(event);
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn winapi_timeout_ms(debounce_ms: u64) -> u32 {
    // Wait slightly longer than the debounce period to catch coalesced events
    (debounce_ms as u32).saturating_add(50).min(5000)
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
    add_watch_recursive(fd, root, &mut watch_map, config);

    if watch_map.is_empty() {
        unsafe { libc::close(fd) };
        return Err("No directories to watch".into());
    }

    info!(
        "Native Linux inotify watcher started on {} ({} dirs)",
        root.display(),
        watch_map.len()
    );

    let mut buffer = vec![0u8; 4096];
    let mut debounce_path: Option<PathBuf> = None;
    let mut debounce_time: Option<Instant> = None;
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
                if should_watch(path, config) {
                    let _ = tx.send(FileEvent {
                        path: path.clone(),
                        kind: EventKind::Modify,
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
                add_watch_recursive(fd, root, &mut watch_map, config);
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

                if should_watch(&full_path, config) {
                    let now = Instant::now();
                    if let (Some(prev_path), Some(prev_time)) = (&debounce_path, debounce_time)
                        && (prev_path != &full_path || now.duration_since(prev_time) > debounce_dur)
                    {
                        let _ = tx.send(FileEvent {
                            path: prev_path.clone(),
                            kind,
                        });
                    }
                    debounce_path = Some(full_path.clone());
                    debounce_time = Some(now);
                }

                // If a new directory was created, add a watch for it
                if kind == EventKind::Create && full_path.is_dir() {
                    add_watch_recursive(fd, &full_path, &mut watch_map, config);
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
            if should_watch(path, config) {
                let _ = tx.send(FileEvent {
                    path: path.clone(),
                    kind: EventKind::Modify,
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
    dir: &Path,
    watch_map: &mut std::collections::HashMap<std::os::unix::io::RawFd, PathBuf>,
    config: &WatcherConfig,
) {
    // Check ignore list
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if config.ignore_dirs.contains(&dir_name.to_string()) {
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
                add_watch_recursive(fd, &path, watch_map, config);
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
        root.display()
    );

    let mut debounce_path: Option<PathBuf> = None;
    let mut debounce_time: Option<Instant> = None;
    let debounce_dur = Duration::from_millis(config.debounce_ms);

    loop {
        match notify_rx.recv_timeout(debounce_dur) {
            Ok(Ok(event)) => {
                if let NotifyEventKind::Modify(_) | NotifyEventKind::Create(_) = event.kind {
                    for path in &event.paths {
                        if should_watch(path, config) {
                            let now = Instant::now();
                            if let (Some(prev_path), Some(prev_time)) =
                                (&debounce_path, debounce_time)
                                && (prev_path != path
                                    || now.duration_since(prev_time) > debounce_dur)
                            {
                                let _ = tx.send(FileEvent {
                                    path: prev_path.clone(),
                                    kind: EventKind::Modify,
                                });
                            }
                            debounce_path = Some(path.clone());
                            debounce_time = Some(now);
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
                    if should_watch(path, config) {
                        let _ = tx.send(FileEvent {
                            path: path.clone(),
                            kind: EventKind::Modify,
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
        root.display()
    );

    loop {
        match debounce_rx.recv_timeout(debounce_dur * 2) {
            Ok(events) => {
                // notify-debouncer coalesces events; emit a FileEvent for each relevant path
                for event in &events {
                    for path in &event.paths {
                        if should_watch(path, config) {
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
