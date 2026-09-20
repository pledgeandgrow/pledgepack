// pledge-native-sys: Rust FFI bindings to the Zig native library
// Links against libpledge_native.a (compiled from Zig source)

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::os::raw::{c_int, c_void};

// ─── Zig runtime symbol stubs (MSVC-target only) ───
// These symbols are referenced by Zig's compiler-rt but not bundled in
// static libraries when targeting MSVC — its linker needs them resolved
// manually. On the GNU/MinGW target these same symbol names are already
// provided by the MinGW runtime's own CRT objects (MinGW-w64 ships its own
// `__stack_chk_guard`/`__stack_chk_fail`/`___chkstk_ms`), so this module
// used to define them unconditionally regardless of target_env — meaning on
// a GNU build, native-sys's own (Rust-defined) versions and MinGW's
// existing runtime versions of the *same symbol names* both existed in the
// link, and the linker had to arbitrarily pick one. This is the confirmed
// root cause of a real, reproducible segfault: PRODUCTION-READINESS-100.md
// goal 93/99 bisected a crash (any code executed on a rayon worker thread —
// even a trivial closure with zero calls into the Zig library — segfaults
// with STATUS_ACCESS_VIOLATION) down to exactly this module being linked
// into a GNU-target binary at all, confirmed via a minimal repro
// (native-sys/examples/minimal_repro.rs) that isolated it from every other
// candidate (mimalloc, tokio, rquickjs, thread creation in general — all
// ruled out individually). Gating these to `target_env = "msvc"` only,
// matching what the comments here already claimed was the actual need,
// removes the duplicate/conflicting definitions on the GNU target and lets
// MinGW's own correct runtime implementations resolve those symbols
// instead.
#[cfg(target_env = "msvc")]
mod msvc_runtime_stubs {
    use super::c_void;

    /// Stack canary guard value (Zig runtime expects this to exist as a symbol).
    ///
    /// Initialized to a fallback constant so the symbol is always valid, but
    /// `init_stack_canary()` should be called once at program startup to replace
    /// it with a value derived from the current time and the symbol's own address
    /// (for ASLR entropy). This makes the canary unpredictable to attackers,
    /// unlike a hardcoded constant.
    #[unsafe(no_mangle)]
    pub static mut __stack_chk_guard: u64 = 0xdeadbeef_cafebabe;

    static STACK_CHK_INIT: std::sync::Once = std::sync::Once::new();

    /// Initialize the stack canary with a random value derived from the current
    /// monotonic time and the static's own address (for ASLR-derived entropy).
    /// Should be called once at program startup; subsequent calls are no-ops.
    /// If never called, the fallback constant `0xdeadbeef_cafebabe` is used.
    pub fn init_stack_canary() {
        STACK_CHK_INIT.call_once(|| {
            let time_canary = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xdeadbeef_cafebabe);
            // XOR with the static's address for additional ASLR-derived entropy
            let addr = std::ptr::addr_of_mut!(__stack_chk_guard) as u64;
            // SAFETY: this runs exactly once under STACK_CHK_INIT, and no stack-
            // protected Zig code runs concurrently during early startup.
            unsafe {
                __stack_chk_guard = time_canary ^ addr;
            }
        });
    }

    /// Called when stack canary check fails — abort the process
    #[unsafe(no_mangle)]
    pub extern "C" fn __stack_chk_fail() -> ! {
        std::process::abort();
    }

    // ___chkstk_ms: Proper stack probe implementation.
    // Zig/LLVM calls this with the stack allocation size in RAX.
    // It must touch each 4096-byte page from RSP down to RSP - RAX
    // to ensure the OS commits the stack pages. RSP and RAX must be preserved.
    #[cfg(target_arch = "x86_64")]
    core::arch::global_asm!(
        ".globl ___chkstk_ms",
        "___chkstk_ms:",
        "mov r10, rax",    // r10 = remaining = size (r10 is volatile)
        "mov rcx, rsp",    // rcx = cursor = rsp (rcx is volatile)
        "cmp r10, 0x1000", // if remaining < page_size, skip loop
        "jb 2f",
        "1:",              // probe full pages
        "sub rcx, 0x1000", // cursor -= 4096
        "test [rcx], al",  // touch page at cursor
        "sub r10, 0x1000", // remaining -= 4096
        "cmp r10, 0x1000", // more than a page left?
        "ja 1b",
        "2:",             // probe final partial page
        "sub rcx, r10",   // cursor -= remaining
        "test [rcx], al", // touch final page
        "ret",            // rax preserved (never modified), rsp preserved
    );

    /// LdrRegisterDllNotification — Windows ntdll function for DLL load notifications
    /// Not available in MSVC 2019 import libraries; provide a no-op stub
    #[unsafe(no_mangle)]
    pub extern "system" fn LdrRegisterDllNotification(
        _flags: u32,
        _callback: *const c_void,
        _context: *const c_void,
        _cookie: *mut c_void,
    ) -> i32 {
        // Return STATUS_SUCCESS (0)
        0
    }
}

#[cfg(target_env = "msvc")]
pub use msvc_runtime_stubs::init_stack_canary;

/// On non-MSVC targets (GNU/MinGW, and every non-Windows target this crate
/// happens to build on), the runtime already provides its own stack canary
/// — there is nothing for this crate to initialize. Kept as a real function
/// (not conditionally compiled away entirely) so every call site can call
/// `pledgepack_native_sys::init_stack_canary()` unconditionally regardless
/// of target.
#[cfg(not(target_env = "msvc"))]
pub fn init_stack_canary() {}

/// Upper bound on the element count of any caller-supplied output buffer we
/// allocate for the Zig library (16Mi entries: 64 MiB of `u32` / 256 MiB of
/// 16-byte task ids). Query capacities come from callers and used to reach
/// `vec![0; capacity]` unchecked - `usize::MAX` aborted the process with a
/// capacity overflow and merely-huge values attempted multi-GiB allocations.
/// No graph query can legitimately return more entries than this (per-node
/// edge counts are capped at 32767 by the Zig side and node ids are `u32`).
pub const MAX_QUERY_CAPACITY: usize = 1 << 24;

/// Reject paths the C/Win32 APIs would silently truncate at an embedded NUL
/// (opening a different file than the one asked for).
fn check_path(path: &str) -> anyhow::Result<()> {
    if path.as_bytes().contains(&0) {
        anyhow::bail!("path contains an embedded NUL byte: {:?}", path);
    }
    Ok(())
}

// Opaque pointer to the Zig ModuleGraph
pub type ModuleGraphHandle = *mut c_void;
// Opaque pointer to the Zig TaskGraph (content-addressed task dependency graph)
pub type TaskGraphHandle = *mut c_void;

unsafe extern "C" {
    // Graph operations
    pub fn pledge_graph_create() -> ModuleGraphHandle;
    pub fn pledge_graph_destroy(g: ModuleGraphHandle);
    pub fn pledge_graph_add_module(
        g: ModuleGraphHandle,
        path_ptr: *const u8,
        path_len: usize,
    ) -> u32;
    pub fn pledge_graph_add_dependency(g: ModuleGraphHandle, from: u32, to: u32);
    pub fn pledge_graph_get_dependents(
        g: ModuleGraphHandle,
        module_id: u32,
        out_ids: *mut u32,
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_graph_get_dependencies(
        g: ModuleGraphHandle,
        module_id: u32,
        out_ids: *mut u32,
        out_capacity: usize,
    ) -> usize;

    // Task graph operations (16-byte blake3 TaskIds across the ABI)
    pub fn pledge_task_graph_create() -> TaskGraphHandle;
    pub fn pledge_task_graph_destroy(g: TaskGraphHandle);
    pub fn pledge_task_graph_add_task(g: TaskGraphHandle, id_ptr: *const [u8; 16]);
    pub fn pledge_task_graph_add_edge(
        g: TaskGraphHandle,
        parent_ptr: *const [u8; 16],
        child_ptr: *const [u8; 16],
    );
    pub fn pledge_task_graph_get_dependents(
        g: TaskGraphHandle,
        id_ptr: *const [u8; 16],
        out_ids: *mut [u8; 16],
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_task_graph_get_dependencies(
        g: TaskGraphHandle,
        id_ptr: *const [u8; 16],
        out_ids: *mut [u8; 16],
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_task_graph_set_status(g: TaskGraphHandle, id_ptr: *const [u8; 16], status: u8);
    pub fn pledge_task_graph_get_status(g: TaskGraphHandle, id_ptr: *const [u8; 16]) -> u8;
    pub fn pledge_task_graph_count(g: TaskGraphHandle) -> usize;
    pub fn pledge_task_graph_mark_dirty(
        g: TaskGraphHandle,
        id_ptr: *const [u8; 16],
        out_ids: *mut [u8; 16],
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_task_graph_ids_by_status(
        g: TaskGraphHandle,
        status: u8,
        out_ids: *mut [u8; 16],
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_task_graph_all_ids(
        g: TaskGraphHandle,
        out_ids: *mut [u8; 16],
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_task_graph_clear(g: TaskGraphHandle);

    // I/O operations
    pub fn pledge_io_read_file(
        path_ptr: *const u8,
        path_len: usize,
        out_buf: *mut *mut u8,
        out_len: *mut usize,
    ) -> c_int;
    pub fn pledge_io_read_files_batch(
        paths_ptr: *const *const u8,
        paths_len_ptr: *const usize,
        count: usize,
        out_bufs: *mut *mut u8,
        out_lens: *mut usize,
    ) -> c_int;
    pub fn pledge_io_free(buf: *mut u8, len: usize);

    // SIMD scanning
    pub fn pledge_simd_find_imports(
        source_ptr: *const u8,
        source_len: usize,
        out_offsets: *mut usize,
        out_capacity: usize,
    ) -> usize;
    pub fn pledge_simd_summarize_module(
        source_ptr: *const u8,
        source_len: usize,
        out_summary: *mut ModuleSummary,
        out_offsets: *mut usize,
        out_capacity: usize,
    ) -> usize;
}

/// One-pass module scan result — mirrors `ModuleSummary` in
/// native-sys/zig/simd.zig. `repr(C)` for a stable FFI layout.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ModuleSummary {
    /// 128-bit content hash (simdHash128-compatible).
    pub content_hash: [u8; 16],
    /// `import`/`export` keyword candidates recorded in the offsets vec.
    pub import_candidate_count: u32,
    /// `import(` expressions (subset of the candidates).
    pub dynamic_import_count: u32,
    /// `require(` calls.
    pub require_count: u32,
    /// `export` keyword occurrences.
    pub export_count: u32,
    /// Bit flags — see the `SUMMARY_*` constants.
    pub flags: u32,
}

/// `'use client'` seen (ModuleSummary::flags).
pub const SUMMARY_USE_CLIENT: u32 = 1 << 0;
/// `'use server'` seen (ModuleSummary::flags).
pub const SUMMARY_USE_SERVER: u32 = 1 << 1;
/// `require()`/`module.exports`/`exports.x` seen — CJS interop hint.
pub const SUMMARY_USES_COMMONJS: u32 = 1 << 2;
/// `console.`/`process.env`/`window`/`document`/`globalThis` seen — a
/// side-effect *hint*; the optimizer's own analysis stays authoritative.
pub const SUMMARY_SIDE_EFFECT_HINT: u32 = 1 << 3;

/// One-pass module summary: scans `source` once and returns the
/// classification struct plus offsets of `import`/`export` keyword
/// candidates (static imports, `import()` expressions, and re-exports —
/// `import.meta` and ident-suffix false hits are already filtered).
///
/// Feeds the offsets to `extract_module_specifier` exactly like
/// `find_imports`, with two improvements: `export ... from` re-exports are
/// now discovered as dependency candidates, and the flags/counts/hash ride
/// along for free.
pub fn summarize_module(source: &[u8]) -> (ModuleSummary, Vec<usize>) {
    let mut summary = ModuleSummary::default();
    let mut capacity = 256usize;
    loop {
        let mut offsets = vec![0usize; capacity];
        let count = unsafe {
            pledge_simd_summarize_module(
                source.as_ptr(),
                source.len(),
                &mut summary,
                offsets.as_mut_ptr(),
                capacity,
            )
        };
        if count < capacity {
            offsets.truncate(count);
            return (summary, offsets);
        }
        // At most one candidate per source byte, so `len + 1` always fits.
        capacity = capacity
            .saturating_mul(4)
            .min(source.len().saturating_add(1));
    }
}

/// RAII wrapper for ModuleGraph
pub struct Graph {
    handle: ModuleGraphHandle,
    /// One past the highest module id handed out by `add_module`. The Zig
    /// side indexes its module array with caller-supplied ids and does no
    /// bounds checking in release builds, so the safe wrappers below validate
    /// ids against this before crossing the FFI boundary.
    module_count: std::cell::Cell<u32>,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl Graph {
    pub fn new() -> Self {
        unsafe {
            let handle = pledge_graph_create();
            // Validate the handle returned by Zig isn't null — a null handle
            // would cause UB on every subsequent operation.
            assert!(
                !handle.is_null(),
                "pledge_graph_create returned a null handle"
            );
            Self {
                handle,
                module_count: std::cell::Cell::new(0),
            }
        }
    }

    pub fn add_module(&self, path: &str) -> u32 {
        let id = unsafe { pledge_graph_add_module(self.handle, path.as_ptr(), path.len()) };
        self.module_count
            .set(self.module_count.get().max(id.saturating_add(1)));
        id
    }

    /// Add a forward edge `from -> to`.
    ///
    /// # Panics
    /// If either id was not returned by [`Graph::add_module`] — passing an
    /// unknown id used to index out of bounds inside the Zig library (UB).
    pub fn add_dependency(&self, from: u32, to: u32) {
        let count = self.module_count.get();
        assert!(
            from < count && to < count,
            "Graph::add_dependency: unknown module id (from={from}, to={to}, modules={count})"
        );
        unsafe { pledge_graph_add_dependency(self.handle, from, to) }
    }

    /// Dependents of `module_id`; empty for an id that was never added.
    pub fn get_dependents(&self, module_id: u32, capacity: usize) -> Vec<u32> {
        if module_id >= self.module_count.get() || capacity == 0 {
            return Vec::new();
        }
        let capacity = capacity.min(MAX_QUERY_CAPACITY);
        let mut ids = vec![0u32; capacity];
        let count = unsafe {
            pledge_graph_get_dependents(self.handle, module_id, ids.as_mut_ptr(), capacity)
        };
        ids.truncate(count);
        ids
    }

    /// Get the dependencies of a module (modules it imports — forward edges).
    ///
    /// `capacity` is the size of the output buffer — the FFI returns at most
    /// `capacity` entries, so a module with more direct dependencies is
    /// silently truncated. Prefer [`Graph::get_all_dependencies`] unless you
    /// have a hard bound.
    pub fn get_dependencies(&self, module_id: u32, capacity: usize) -> Vec<u32> {
        if module_id >= self.module_count.get() || capacity == 0 {
            return Vec::new();
        }
        let capacity = capacity.min(MAX_QUERY_CAPACITY);
        let mut ids = vec![0u32; capacity];
        let count = unsafe {
            pledge_graph_get_dependencies(self.handle, module_id, ids.as_mut_ptr(), capacity)
        };
        ids.truncate(count);
        ids
    }

    /// Get all dependents of a module without truncation.
    ///
    /// The FFI writes at most `capacity` entries per call and returns the
    /// number written, so a full buffer means there may be more. This helper
    /// doubles the buffer until the result fits. (Callers must NOT pass
    /// `usize::MAX` to the raw accessors — that would try to allocate an
    /// impossibly large vector.)
    pub fn get_all_dependents(&self, module_id: u32) -> Vec<u32> {
        let mut capacity = 256usize;
        loop {
            let ids = self.get_dependents(module_id, capacity);
            if ids.len() < capacity || capacity >= MAX_QUERY_CAPACITY {
                return ids;
            }
            capacity = capacity.saturating_mul(2).min(MAX_QUERY_CAPACITY);
        }
    }

    /// Get all dependencies of a module without truncation.
    ///
    /// See [`Graph::get_all_dependents`] for why the buffer is grown
    /// incrementally instead of passing a huge capacity.
    pub fn get_all_dependencies(&self, module_id: u32) -> Vec<u32> {
        let mut capacity = 256usize;
        loop {
            let ids = self.get_dependencies(module_id, capacity);
            if ids.len() < capacity || capacity >= MAX_QUERY_CAPACITY {
                return ids;
            }
            capacity = capacity.saturating_mul(2).min(MAX_QUERY_CAPACITY);
        }
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        unsafe { pledge_graph_destroy(self.handle) }
    }
}

// Safety: Graph owns its Zig-allocated memory and is not shared across threads.
// Only Send is implemented — concurrent access would require synchronization
// in the Zig code, which is not guaranteed.
unsafe impl Send for Graph {}
// NOTE: Sync deliberately NOT implemented. Use a Mutex<Graph> if shared access is needed.

/// RAII wrapper for the Zig TaskGraph (arena-allocated, 16-byte TaskIds).
///
/// The underlying Zig graph has no internal locking — all methods take
/// `&self` here but callers MUST serialize mutations externally
/// (`Mutex<TaskGraph>`) when sharing across threads.
pub struct TaskGraph {
    handle: TaskGraphHandle,
}

impl Default for TaskGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskGraph {
    pub fn new() -> Self {
        unsafe {
            let handle = pledge_task_graph_create();
            assert!(
                !handle.is_null(),
                "pledge_task_graph_create returned a null handle"
            );
            Self { handle }
        }
    }

    /// Add a task (no-op if it already exists).
    pub fn add_task(&self, id: &[u8; 16]) {
        unsafe { pledge_task_graph_add_task(self.handle, id) }
    }

    /// Add an edge parent→child, creating both tasks if absent.
    pub fn add_edge(&self, parent: &[u8; 16], child: &[u8; 16]) {
        unsafe { pledge_task_graph_add_edge(self.handle, parent, child) }
    }

    /// Grow-until-it-fits query for the capacity-based FFI accessors.
    /// The FFI returns at most `capacity` entries; a full buffer means the
    /// result may be truncated, so double and retry.
    fn query_ids(
        &self,
        start_capacity: usize,
        mut f: impl FnMut(*mut [u8; 16], usize) -> usize,
    ) -> Vec<[u8; 16]> {
        let mut capacity = start_capacity.clamp(1, MAX_QUERY_CAPACITY);
        loop {
            let mut ids = vec![[0u8; 16]; capacity];
            let count = f(ids.as_mut_ptr(), capacity);
            if count < capacity || capacity >= MAX_QUERY_CAPACITY {
                ids.truncate(count);
                return ids;
            }
            capacity = capacity.saturating_mul(4).min(MAX_QUERY_CAPACITY);
        }
    }

    /// Tasks that depend on `id` (reverse edges).
    pub fn dependents(&self, id: &[u8; 16]) -> Vec<[u8; 16]> {
        self.query_ids(256, |out, cap| unsafe {
            pledge_task_graph_get_dependents(self.handle, id, out, cap)
        })
    }

    /// Tasks that `id` depends on (forward edges).
    pub fn dependencies(&self, id: &[u8; 16]) -> Vec<[u8; 16]> {
        self.query_ids(256, |out, cap| unsafe {
            pledge_task_graph_get_dependencies(self.handle, id, out, cap)
        })
    }

    /// Set a task's status (Zig TaskStatus: 0 clean, 1 dirty, 2 computing,
    /// 3 error, 4 pending, 5 evicted). No-op for unknown ids.
    pub fn set_status(&self, id: &[u8; 16], status: u8) {
        unsafe { pledge_task_graph_set_status(self.handle, id, status) }
    }

    /// Get a task's status; unknown ids report 4 (pending).
    pub fn status(&self, id: &[u8; 16]) -> u8 {
        unsafe { pledge_task_graph_get_status(self.handle, id) }
    }

    pub fn task_count(&self) -> usize {
        unsafe { pledge_task_graph_count(self.handle) }
    }

    /// Mark `id` and all transitive dependents dirty in a single FFI call.
    /// Returns every dirtied TaskId.
    pub fn mark_dirty(&self, id: &[u8; 16]) -> Vec<[u8; 16]> {
        self.query_ids(1024, |out, cap| unsafe {
            pledge_task_graph_mark_dirty(self.handle, id, out, cap)
        })
    }

    /// Flat status scan — all ids whose packed status matches `status`.
    pub fn ids_by_status(&self, status: u8) -> Vec<[u8; 16]> {
        let n = self.task_count().max(16);
        self.query_ids(n, |out, cap| unsafe {
            pledge_task_graph_ids_by_status(self.handle, status, out, cap)
        })
    }

    /// Every TaskId in the graph.
    pub fn all_ids(&self) -> Vec<[u8; 16]> {
        let n = self.task_count().max(16);
        self.query_ids(n, |out, cap| unsafe {
            pledge_task_graph_all_ids(self.handle, out, cap)
        })
    }

    /// Free the arena and re-initialize the graph in place.
    pub fn clear(&self) {
        unsafe { pledge_task_graph_clear(self.handle) }
    }
}

impl Drop for TaskGraph {
    fn drop(&mut self) {
        unsafe { pledge_task_graph_destroy(self.handle) }
    }
}

// Safety: TaskGraph owns its Zig-allocated memory; callers must serialize
// concurrent access externally (Mutex) — the Zig side has no locking.
unsafe impl Send for TaskGraph {}
// NOTE: Sync deliberately NOT implemented. Wrap in a Mutex to share.

/// Read a file using the Zig I/O layer
pub fn read_file(path: &str) -> anyhow::Result<Vec<u8>> {
    check_path(path)?;
    let mut buf_ptr: *mut u8 = std::ptr::null_mut();
    let mut buf_len: usize = 0;

    let result = unsafe {
        pledge_io_read_file(
            path.as_ptr(),
            path.len(),
            &mut buf_ptr as *mut *mut u8,
            &mut buf_len as *mut usize,
        )
    };

    if result != 0 {
        // On error, Zig may still have allocated a buffer — free it if so.
        if !buf_ptr.is_null() {
            unsafe { pledge_io_free(buf_ptr, buf_len) };
        }
        anyhow::bail!("Failed to read file: {}", path);
    }

    // Validate the pointer returned by Zig.
    if buf_ptr.is_null() {
        // Empty file or Zig returned null — nothing to free.
        return Ok(Vec::new());
    }

    // Copy from Zig-allocated buffer to a Rust-owned Vec, then free the Zig
    // buffer to avoid leaking memory on every file read.
    let data = unsafe {
        let slice = std::slice::from_raw_parts(buf_ptr, buf_len);
        let vec = slice.to_vec();
        // Free the Zig-allocated buffer regardless of len.
        pledge_io_free(buf_ptr, buf_len);
        vec
    };

    Ok(data)
}

/// Batch-read multiple files in one FFI call via the platform-optimized
/// Zig path: a real I/O completion port on Windows, io_uring on Linux,
/// and a thread pool elsewhere.
///
/// Returns one `Result` per input path — individual failures are isolated
/// (a failed file yields `Err` at its index without failing the batch).
/// Buffers are copied into Rust-owned `Vec`s and each Zig-side buffer is
/// released (`pledge_io_free`) as soon as it has been copied.
pub fn read_files_batch(paths: &[&str]) -> Vec<anyhow::Result<Vec<u8>>> {
    if paths.is_empty() {
        return Vec::new();
    }
    let ptrs: Vec<*const u8> = paths.iter().map(|p| p.as_ptr()).collect();
    let lens: Vec<usize> = paths.iter().map(|p| p.len()).collect();
    let mut bufs: Vec<*mut u8> = vec![std::ptr::null_mut(); paths.len()];
    let mut out_lens: Vec<usize> = vec![0; paths.len()];

    let rc = unsafe {
        pledge_io_read_files_batch(
            ptrs.as_ptr(),
            lens.as_ptr(),
            paths.len(),
            bufs.as_mut_ptr(),
            out_lens.as_mut_ptr(),
        )
    };

    paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let ptr = bufs[i];
            // Reject NUL paths per slot (the Zig side rejects them too; this
            // keeps behaviour identical against an older prebuilt library).
            if let Err(e) = check_path(path) {
                if !ptr.is_null() {
                    unsafe { pledge_io_free(ptr, out_lens[i]) };
                }
                return Err(e);
            }
            // Contract: failures leave out_bufs[i] null and out_lens[i]=0.
            if ptr.is_null() {
                if rc == 0 {
                    // Zig reported success but handed back nothing —
                    // treat as an error rather than a bogus empty read.
                    return Err(anyhow::anyhow!("batch read returned null for {}", path));
                }
                return Err(anyhow::anyhow!("failed to read {}", path));
            }
            let len = out_lens[i];
            let data = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
            // The buffer is owned by us now - release it (this used to be
            // skipped, leaking every batch-read buffer).
            unsafe { pledge_io_free(ptr, len) };
            Ok(data)
        })
        .collect()
}

/// Find import statements using SIMD-accelerated scanning.
///
/// Returns byte offsets into `source` where `import` appears. Starts with
/// a small buffer (256 offsets — covers essentially all real modules) and
/// doubles if the FFI fills it, instead of allocating 512KB per call.
///
/// Prefer [`summarize_module`] for new call sites — it does this scan plus
/// directive/CJS flags and a content hash in the same single pass.
pub fn find_imports(source: &[u8]) -> Vec<usize> {
    let mut capacity = 256usize;
    loop {
        let mut offsets = vec![0usize; capacity];
        let count = unsafe {
            pledge_simd_find_imports(
                source.as_ptr(),
                source.len(),
                offsets.as_mut_ptr(),
                capacity,
            )
        };
        if count < capacity {
            offsets.truncate(count);
            return offsets;
        }
        capacity = capacity
            .saturating_mul(4)
            .min(source.len().saturating_add(1));
    }
}
