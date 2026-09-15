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

// Opaque pointer to the Zig ModuleGraph
pub type ModuleGraphHandle = *mut c_void;

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
}

/// RAII wrapper for ModuleGraph
pub struct Graph {
    handle: ModuleGraphHandle,
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
            Self { handle }
        }
    }

    pub fn add_module(&self, path: &str) -> u32 {
        unsafe { pledge_graph_add_module(self.handle, path.as_ptr(), path.len()) }
    }

    pub fn add_dependency(&self, from: u32, to: u32) {
        unsafe { pledge_graph_add_dependency(self.handle, from, to) }
    }

    pub fn get_dependents(&self, module_id: u32, capacity: usize) -> Vec<u32> {
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
            if ids.len() < capacity {
                return ids;
            }
            capacity = capacity.saturating_mul(2);
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
            if ids.len() < capacity {
                return ids;
            }
            capacity = capacity.saturating_mul(2);
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

/// Read a file using the Zig I/O layer
pub fn read_file(path: &str) -> anyhow::Result<Vec<u8>> {
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

/// Find import statements using SIMD-accelerated scanning.
///
/// Returns byte offsets into `source` where `import` appears. The Zig side
/// caps the result count at the supplied capacity, so we allocate a large
/// buffer (65536) to avoid silently truncating files with many imports.
pub fn find_imports(source: &[u8]) -> Vec<usize> {
    const MAX_IMPORTS: usize = 65536;
    let mut offsets = vec![0usize; MAX_IMPORTS];
    let count = unsafe {
        pledge_simd_find_imports(
            source.as_ptr(),
            source.len(),
            offsets.as_mut_ptr(),
            offsets.len(),
        )
    };
    offsets.truncate(count);
    offsets
}
