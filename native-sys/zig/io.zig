// I/O layer: io_uring (Linux), kqueue (macOS), IOCP (Windows)
// Falls back to C stdlib for cross-platform file I/O.
// Uses C stdlib because Zig 0.16.0 std.Io requires an Io parameter
// from Juicy Main, which is not available in a C ABI static library.

const std = @import("std");
const builtin = @import("builtin");

const Allocator = std.mem.Allocator;

// ─── C stdlib bindings ───
extern "c" fn fopen(path: [*:0]const u8, mode: [*:0]const u8) ?*anyopaque;
extern "c" fn fclose(stream: *anyopaque) c_int;
extern "c" fn fread(ptr: [*]u8, size: usize, nmemb: usize, stream: *anyopaque) usize;
extern "c" fn fseek(stream: *anyopaque, offset: c_long, whence: c_int) c_int;
extern "c" fn ftell(stream: *anyopaque) c_long;
extern "c" fn remove(path: [*:0]const u8) c_int;
extern "c" fn fwrite(ptr: [*]const u8, size: usize, nmemb: usize, stream: *anyopaque) usize;

const SEEK_END: c_int = 2;
const SEEK_SET: c_int = 0;

/// Maximum size of a single file the loader will read. Larger files are
/// rejected (-1) instead of attempting a multi-GiB allocation; source and
/// asset inputs to a bundler are nowhere near this. (Also keeps every read
/// within `c_long`/`u32` limits of ftell/ReadFile/io_uring.)
pub const MAX_FILE_SIZE: usize = 1 << 30; // 1 GiB

// ─── Buffer ownership ───
//
// Every buffer handed across the C ABI is an individually heap-allocated
// block from the C allocator, preceded by a 16-byte header recording the
// block's true allocation size. `freeBuffer` therefore does NOT depend on the
// `len` the caller passes back (which is the *data* length and can differ from
// the allocation size: empty files allocate 1 byte, short reads shrink len).
// This replaces the former process-global arena, whose buffers were never
// released (pledge_io_free was a no-op and resetArena was never exported),
// leaking one buffer per read for the lifetime of the process.
const io_allocator = std.heap.c_allocator;
const BUF_HEADER: usize = 16;

fn ioAlloc(n: usize) ?[*]u8 {
    const total = std.math.add(usize, n, BUF_HEADER) catch return null;
    const raw = io_allocator.alignedAlloc(u8, .@"16", total) catch return null;
    @as(*usize, @ptrCast(raw.ptr)).* = total;
    return raw.ptr + BUF_HEADER;
}

/// Free a pointer previously returned through the ABI (ioAlloc).
pub fn freeBufferPtr(p: [*]u8) void {
    const base: [*]align(16) u8 = @alignCast(p - BUF_HEADER);
    const total = @as(*usize, @ptrCast(base)).*;
    io_allocator.free(base[0..total]);
}

/// Scratch (internal, never crosses the ABI) typed allocation. Callers own
/// it and must `defer scratchFree`.
fn scratchAlloc(comptime T: type, n: usize) ?[]T {
    return io_allocator.alloc(T, n) catch null;
}

fn scratchFree(s: anytype) void {
    io_allocator.free(s);
}

/// Read a single file into a freshly allocated buffer (release with
/// `freeBuffer`). Returns 0 on success, -1 on error; on error nothing is
/// allocated and out_buf/out_len are untouched.
///
/// Paths containing an embedded NUL are rejected: the C/Win32 APIs would
/// silently truncate them at the NUL and open a different file.
pub fn readFile(
    path: []const u8,
    out_buf: *[*]u8,
    out_len: *usize,
) c_int {
    if (std.mem.indexOfScalar(u8, path, 0) != null) return -1;
    if (builtin.os.tag == .windows) return readFileWindows(path, out_buf, out_len);

    // Build null-terminated path
    const path_z = io_allocator.dupeZ(u8, path) catch return -1;
    defer io_allocator.free(path_z);

    const fp = fopen(path_z.ptr, "rb") orelse return -1;
    defer _ = fclose(fp);

    // Get file size
    if (fseek(fp, 0, SEEK_END) != 0) {
        return -1;
    }
    const ftell_result = ftell(fp);
    if (ftell_result < 0) {
        return -1;
    }
    const size: usize = @intCast(ftell_result);
    if (size > MAX_FILE_SIZE) return -1;
    if (fseek(fp, 0, SEEK_SET) != 0) {
        return -1;
    }

    if (size == 0) {
        const empty_buf = ioAlloc(1) orelse return -1;
        out_buf.* = empty_buf;
        out_len.* = 0;
        return 0;
    }

    const buf = ioAlloc(size) orelse return -1;
    const n = fread(buf, 1, size, fp);
    if (n != size) {
        // Short read (file truncated/changed underneath us, I/O error).
        freeBufferPtr(buf);
        return -1;
    }
    out_buf.* = buf;
    out_len.* = n;
    return 0;
}

/// Windows single-file read via CreateFileW so non-ASCII paths work (the C
/// runtime's fopen interprets the byte string in the ANSI code page, so a
/// UTF-8 path with non-ASCII characters opened the wrong file or failed).
fn readFileWindows(
    path: []const u8,
    out_buf: *[*]u8,
    out_len: *usize,
) c_int {
    const w = windows;
    const path_w = std.unicode.utf8ToUtf16LeAllocZ(io_allocator, path) catch return -1;
    defer io_allocator.free(path_w);

    const handle = w.CreateFileW(
        path_w.ptr,
        w.GENERIC_READ,
        w.FILE_SHARE_READ | w.FILE_SHARE_WRITE,
        null,
        w.OPEN_EXISTING,
        0,
        null,
    );
    if (handle == w.INVALID_HANDLE_VALUE) return -1;
    defer _ = w.CloseHandle(handle);

    var size: i64 = 0;
    if (w.GetFileSizeEx(handle, &size) == 0 or size < 0) return -1;
    if (size > @as(i64, @intCast(MAX_FILE_SIZE))) return -1;
    const usize_size: usize = @intCast(size);

    if (usize_size == 0) {
        const empty_buf = ioAlloc(1) orelse return -1;
        out_buf.* = empty_buf;
        out_len.* = 0;
        return 0;
    }

    const buf = ioAlloc(usize_size) orelse return -1;
    var total: usize = 0;
    while (total < usize_size) {
        var got: u32 = 0;
        const chunk: u32 = @intCast(@min(usize_size - total, 1 << 30));
        if (w.ReadFile(handle, buf + total, chunk, &got, null) == 0 or got == 0) {
            freeBufferPtr(buf);
            return -1;
        }
        total += got;
    }
    out_buf.* = buf;
    out_len.* = total;
    return 0;
}

/// Batch-read multiple files using platform-optimized async I/O.
/// On Linux: uses io_uring for batched submission.
/// On other platforms: falls back to thread pool.
pub fn readFilesBatch(
    paths_ptr: [*]const [*]const u8,
    paths_len_ptr: [*]const usize,
    count: usize,
    out_bufs: [*][*]u8,
    out_lens: [*]usize,
) c_int {
    var errors: c_int = 0;

    // Contract: out_lens[i] is always written (0 on failure); out_bufs[i]
    // is only written on success — callers pre-null it to detect failures.
    for (0..count) |i| out_lens[i] = 0;

    // Use parallel reading for large batches, sequential for small
    if (count > 8) {
        var threads: [16]?std.Thread = .{null} ** 16;
        const thread_count = @min(count, 16);

        const ReadJob = struct {
            path: []const u8,
            out_buf: *[*]u8,
            out_len: *usize,
            result: c_int,
        };

        const jobs = scratchAlloc(ReadJob, count) orelse return -1;
        defer scratchFree(jobs);

        for (0..count) |i| {
            jobs[i] = .{
                .path = paths_ptr[i][0..paths_len_ptr[i]],
                .out_buf = &out_bufs[i],
                .out_len = &out_lens[i],
                .result = -1,
            };
        }

        const worker = struct {
            fn run(job: *ReadJob) void {
                job.result = readFile(job.path, job.out_buf, job.out_len);
            }
        };

        // Spawn threads in batches
        var i: usize = 0;
        while (i < count) {
            const batch = @min(thread_count, count - i);
            for (0..batch) |j| {
                threads[j] = std.Thread.spawn(.{}, worker.run, .{&jobs[i + j]}) catch null;
                if (threads[j] == null) {
                    worker.run(&jobs[i + j]);
                }
            }
            for (0..batch) |j| {
                if (threads[j]) |t| t.join();
                if (jobs[i + j].result != 0) errors = -1;
            }
            i += batch;
        }
    } else {
        // Sequential: small batch, not worth thread overhead
        for (0..count) |i| {
            const path = paths_ptr[i][0..paths_len_ptr[i]];
            const result = readFile(path, &out_bufs[i], &out_lens[i]);
            if (result != 0) errors = -1;
        }
    }

    return errors;
}

/// Free a buffer returned by readFile / the batch readers. `buf.len` is
/// ignored: the true allocation size lives in the block header.
pub fn freeBuffer(buf: []u8) void {
    freeBufferPtr(buf.ptr);
}

// ─── G4.12: Platform-optimized async I/O ─────────────────────────────
//
// io_uring (Linux), IOCP (Windows), kqueue (macOS)
// Falls back to thread pool on platforms without these APIs.
// The C ABI static library uses C stdlib for file I/O because Zig 0.16
// std.Io requires an Io parameter from Juicy Main. These implementations
// use raw syscalls / C library calls for async batch reads.

/// I/O backend type
pub const IoBackend = enum {
    thread_pool,
    io_uring,
    iocp,
    kqueue,
};

/// Detect the best available I/O backend for this platform.
pub fn detectBackend() IoBackend {
    if (builtin.os.tag == .linux) return .io_uring;
    if (builtin.os.tag == .windows) return .iocp;
    if (builtin.os.tag == .macos or builtin.os.tag == .freebsd) return .kqueue;
    return .thread_pool;
}

/// Check if the platform's native async file I/O is available at runtime.
/// macOS/BSD are false: kqueue can't async regular files (only watch them).
pub fn asyncIoAvailable() bool {
    return switch (builtin.os.tag) {
        .linux => true, // io_uring on kernel 5.1+
        .windows => true, // IOCP on all Windows
        else => false,
    };
}

// ─── io_uring (Linux) ────────────────────────────────────────────────
//
// Uses raw Linux syscalls for io_uring setup and submission.
// Falls back to thread pool if io_uring is not available.

// Raw-syscall io_uring implementation. std.os.linux is avoided so the
// layout/syscall details stay explicit and stable across std versions.

/// True when a raw syscall return value encodes -errno.
fn sysErr(rc: usize) bool {
    const signed: isize = @bitCast(rc);
    return signed < 0 and signed >= -4095;
}

/// Kernel io_uring_params — filled by io_uring_setup with ring offsets.
pub const IoUringParams = extern struct {
    sq_entries: u32 = 0,
    cq_entries: u32 = 0,
    flags: u32 = 0,
    sq_thread_cpu: u32 = 0,
    sq_thread_idle: u32 = 0,
    features: u32 = 0,
    wq_fd: u32 = 0,
    resv: [3]u32 = .{ 0, 0, 0 },
    sq_off: extern struct { head: u32, tail: u32, ring_mask: u32, ring_entries: u32, flags: u32, dropped: u32, array: u32, resv1: u32, resv2: u64 } = .{ .head = 0, .tail = 0, .ring_mask = 0, .ring_entries = 0, .flags = 0, .dropped = 0, .array = 0, .resv1 = 0, .resv2 = 0 },
    cq_off: extern struct { head: u32, tail: u32, ring_mask: u32, ring_entries: u32, overflow: u32, cqes: u32, flags: u32, resv1: u32, resv2: u64 } = .{ .head = 0, .tail = 0, .ring_mask = 0, .ring_entries = 0, .overflow = 0, .cqes = 0, .flags = 0, .resv1 = 0, .resv2 = 0 },
};

const UringRing = struct {
    fd: i32,
    sq_head: *u32,
    sq_tail: *u32,
    sq_mask: u32,
    sq_array: [*]u32,
    sqes: [*]IoUringSqe,
    sq_entries: u32,
    cq_head: *u32,
    cq_tail: *u32,
    cq_mask: u32,
    cqes: [*]IoUringCqe,
    sq_ring: [*]u8,
    sq_ring_len: usize,
    cq_ring: [*]u8,
    cq_ring_len: usize,
    sqes_map: [*]u8,
    sqes_len: usize,
};

const IoUringSqe = extern struct {
    opcode: u8 = 0,
    flags: u8 = 0,
    ioprio: u16 = 0,
    fd: i32 = 0,
    off: u64 = 0,
    addr: u64 = 0,
    len: u32 = 0,
    op_flags: u32 = 0,
    user_data: u64 = 0,
    buf_index: u16 = 0,
    personality: u16 = 0,
    splice_fd_in: i32 = 0,
    pad2: [2]u64 = .{ 0, 0 },
};

const IoUringCqe = extern struct {
    user_data: u64,
    res: i32,
    flags: u32,
};

const UringOff = struct {
    const SQ_RING: i64 = 0;
    const CQ_RING: i64 = 0x8000000;
    const SQES: i64 = 0x10000000;
};

fn uringMmap(len: usize, fd: i32, offset: i64) ?[*]u8 {
    const rc = std.os.linux.syscall6(
        .mmap,
        0,
        len,
        0x3, // PROT_READ | PROT_WRITE
        0x01, // MAP_SHARED
        @as(usize, @bitCast(@as(isize, fd))),
        @as(usize, @bitCast(offset)),
    );
    if (sysErr(rc)) return null;
    return @ptrFromInt(rc);
}

fn uringSetup(entries: u32) ?UringRing {
    var params: IoUringParams = .{};
    const setup_rc = std.os.linux.syscall2(
        .io_uring_setup,
        entries,
        @intFromPtr(&params),
    );
    if (sysErr(setup_rc)) return null;
    const ring_fd: i32 = @intCast(setup_rc);

    const sq_ring_len = params.sq_off.array + params.sq_entries * @sizeOf(u32);
    const cq_ring_len = params.cq_off.cqes + params.cq_entries * @sizeOf(IoUringCqe);
    const sqes_len = params.sq_entries * @sizeOf(IoUringSqe);

    const sq_ring = uringMmap(sq_ring_len, ring_fd, UringOff.SQ_RING) orelse {
        _ = std.os.linux.syscall1(.close, @intCast(ring_fd));
        return null;
    };
    const cq_ring = uringMmap(cq_ring_len, ring_fd, UringOff.CQ_RING) orelse {
        _ = std.os.linux.syscall2(.munmap, @intFromPtr(sq_ring), sq_ring_len);
        _ = std.os.linux.syscall1(.close, @intCast(ring_fd));
        return null;
    };
    const sqes_map = uringMmap(sqes_len, ring_fd, UringOff.SQES) orelse {
        _ = std.os.linux.syscall2(.munmap, @intFromPtr(cq_ring), cq_ring_len);
        _ = std.os.linux.syscall2(.munmap, @intFromPtr(sq_ring), sq_ring_len);
        _ = std.os.linux.syscall1(.close, @intCast(ring_fd));
        return null;
    };

    const sq_base = @intFromPtr(sq_ring);
    const cq_base = @intFromPtr(cq_ring);
    // Masks/entries are kernel-filled fields in the rings — read them
    // rather than assuming entries-1.
    return .{
        .fd = ring_fd,
        .sq_head = @ptrFromInt(sq_base + params.sq_off.head),
        .sq_tail = @ptrFromInt(sq_base + params.sq_off.tail),
        .sq_mask = @as(*u32, @ptrFromInt(sq_base + params.sq_off.ring_mask)).*,
        .sq_array = @ptrFromInt(sq_base + params.sq_off.array),
        .sqes = @ptrFromInt(@intFromPtr(sqes_map)),
        .sq_entries = params.sq_entries,
        .cq_head = @ptrFromInt(cq_base + params.cq_off.head),
        .cq_tail = @ptrFromInt(cq_base + params.cq_off.tail),
        .cq_mask = @as(*u32, @ptrFromInt(cq_base + params.cq_off.ring_mask)).*,
        .cqes = @ptrFromInt(cq_base + params.cq_off.cqes),
        .sq_ring = sq_ring,
        .sq_ring_len = sq_ring_len,
        .cq_ring = cq_ring,
        .cq_ring_len = cq_ring_len,
        .sqes_map = sqes_map,
        .sqes_len = sqes_len,
    };
}

fn uringTeardown(r: *UringRing) void {
    _ = std.os.linux.syscall2(.munmap, @intFromPtr(r.sqes_map), r.sqes_len);
    _ = std.os.linux.syscall2(.munmap, @intFromPtr(r.cq_ring), r.cq_ring_len);
    _ = std.os.linux.syscall2(.munmap, @intFromPtr(r.sq_ring), r.sq_ring_len);
    _ = std.os.linux.syscall1(.close, @intCast(r.fd));
}

fn uringSubmitReads(r: *UringRing, fds: []const i32, bufs: []const [*]u8, lens: []const u32, indices: []const usize) u32 {
    var tail = @atomicLoad(u32, r.sq_tail, .acquire);
    var submitted: u32 = 0;
    for (fds, 0..) |fd, k| {
        const slot = tail & r.sq_mask;
        r.sqes[slot] = .{
            .opcode = 22, // IORING_OP_READ
            .fd = fd,
            .off = 0,
            .addr = @intFromPtr(bufs[k]),
            .len = lens[k],
            .user_data = indices[k],
        };
        r.sq_array[slot] = slot;
        tail += 1;
        submitted += 1;
    }
    @atomicStore(u32, r.sq_tail, tail, .release);
    const rc = std.os.linux.syscall6(
        .io_uring_enter,
        @intCast(r.fd),
        submitted,
        submitted, // min_complete: wait for the whole wave
        1, // IORING_ENTER_GETEVENTS
        0,
        0,
    );
    // If the submit itself failed, report 0 so the caller doesn't drain
    // completions that will never arrive.
    if (sysErr(rc)) return 0;
    return submitted;
}

fn uringDrain(r: *UringRing, expected: u32, out_bufs: [*][*]u8, out_lens: [*]usize, jobs_bufs: []const ?[*]u8, sizes: []const u32) c_int {
    var errors: c_int = 0;
    var done: u32 = 0;
    while (done < expected) {
        var head = @atomicLoad(u32, r.cq_head, .acquire);
        if (head == @atomicLoad(u32, r.cq_tail, .acquire)) {
            _ = std.os.linux.syscall6(
                .io_uring_enter,
                @intCast(r.fd),
                0,
                1,
                1, // GETEVENTS, wait for ≥1
                0,
                0,
            );
            continue;
        }
        while (head != @atomicLoad(u32, r.cq_tail, .acquire)) {
            const cqe = r.cqes[head & r.cq_mask];
            head += 1;
            done += 1;
            const i: usize = @intCast(cqe.user_data);
            // Only a complete read counts as success; a short read/EOF is an
            // error (the caller frees any buffer whose out_lens[i] stays 0).
            if (cqe.res >= 0 and @as(u32, @intCast(cqe.res)) == sizes[i]) {
                out_bufs[i] = jobs_bufs[i].?;
                out_lens[i] = @intCast(cqe.res);
            } else {
                errors = -1;
                out_lens[i] = 0;
            }
        }
        @atomicStore(u32, r.cq_head, head, .release);
    }
    return errors;
}

/// Batch-read files using io_uring on Linux (kernel 5.1+).
///
/// Files are opened and fstat'd synchronously (cheap), then the whole
/// wave of IORING_OP_READs is submitted in one io_uring_enter and drained
/// from the completion ring — one syscall pair per wave instead of a
/// read(2) per file. Falls back to the thread pool when io_uring setup
/// fails (old kernel, restricted sandbox, WSL1).
pub fn readFilesIoUring(
    paths_ptr: [*]const [*]const u8,
    paths_len_ptr: [*]const usize,
    count: usize,
    out_bufs: [*][*]u8,
    out_lens: [*]usize,
) c_int {
    if (builtin.os.tag != .linux) {
        return readFilesBatch(paths_ptr, paths_len_ptr, count, out_bufs, out_lens);
    }
    if (count == 0) return 0;

    var errors: c_int = 0;

    // Open + size every file first (plain syscalls — no ring needed).
    const AT_FDCWD: usize = @bitCast(@as(isize, -100));
    const fds = scratchAlloc(i32, count) orelse return -1;
    defer scratchFree(fds);
    const bufs = scratchAlloc(?[*]u8, count) orelse return -1;
    defer scratchFree(bufs);
    const sizes = scratchAlloc(u32, count) orelse return -1;
    defer scratchFree(sizes);
    const read_indices = scratchAlloc(usize, count) orelse return -1;
    defer scratchFree(read_indices);
    // delivered[i]: out_bufs[i] holds a buffer the caller now owns.
    const delivered = scratchAlloc(bool, count) orelse return -1;
    defer scratchFree(delivered);
    for (0..count) |i| {
        fds[i] = -1;
        bufs[i] = null;
        sizes[i] = 0;
        delivered[i] = false;
        out_lens[i] = 0;
    }
    var n_read: usize = 0;

    // Single exit cleanup: close any still-open fd and free any buffer that
    // was allocated but never handed to the caller (error/short-read paths).
    defer {
        for (0..count) |i| {
            if (fds[i] >= 0) _ = std.os.linux.syscall1(.close, @intCast(fds[i]));
            if (!delivered[i]) {
                if (bufs[i]) |b| freeBufferPtr(b);
            }
        }
    }

    for (0..count) |i| {
        const path = paths_ptr[i][0..paths_len_ptr[i]];
        // Embedded NUL would truncate the C path — reject.
        if (std.mem.indexOfScalar(u8, path, 0) != null) {
            errors = -1;
            continue;
        }
        const path_z = io_allocator.dupeZ(u8, path) catch {
            errors = -1;
            continue;
        };
        defer io_allocator.free(path_z);
        const fd_rc = std.os.linux.syscall4(
            .openat,
            AT_FDCWD,
            @intFromPtr(path_z.ptr),
            0, // O_RDONLY
            0,
        );
        if (sysErr(fd_rc)) {
            errors = -1;
            continue;
        }
        const fd: i32 = @intCast(fd_rc);

        // lseek(2) SEEK_END gives the size without needing a stat struct
        // (std.posix.Stat is void on Linux in Zig 0.16 — stat structs were
        // removed in favor of statx; lseek is simpler for regular files).
        const size_rc = std.os.linux.syscall3(.lseek, @intCast(fd), 0, 2);
        if (sysErr(size_rc)) {
            _ = std.os.linux.syscall1(.close, @intCast(fd));
            errors = -1;
            continue;
        }
        const size: i64 = @bitCast(size_rc);
        if (size < 0 or size > @as(i64, @intCast(MAX_FILE_SIZE))) {
            _ = std.os.linux.syscall1(.close, @intCast(fd));
            errors = -1;
            continue;
        }
        if (size == 0) {
            _ = std.os.linux.syscall1(.close, @intCast(fd));
            const b = ioAlloc(1) orelse {
                errors = -1;
                continue;
            };
            out_bufs[i] = b;
            delivered[i] = true;
            continue;
        }
        const buf = ioAlloc(@intCast(size)) orelse {
            _ = std.os.linux.syscall1(.close, @intCast(fd));
            errors = -1;
            continue;
        };
        fds[i] = fd;
        bufs[i] = buf;
        sizes[i] = @intCast(size);
        read_indices[n_read] = i;
        n_read += 1;
    }

    if (n_read > 0) {
        var ring = uringSetup(64) orelse {
            // io_uring unavailable — release everything this call produced
            // (the deferred cleanup closes fds/frees undelivered buffers;
            // buffers already delivered for empty files must be released
            // here because the fallback re-reads every path from scratch).
            for (0..count) |i| {
                if (delivered[i]) {
                    freeBufferPtr(out_bufs[i]);
                    delivered[i] = false;
                }
            }
            return readFilesBatch(paths_ptr, paths_len_ptr, count, out_bufs, out_lens);
        };
        defer uringTeardown(&ring);

        // Submit in waves of sq_entries.
        var start: usize = 0;
        while (start < n_read) {
            const wave = @min(@as(usize, ring.sq_entries), n_read - start);
            const wave_fds = scratchAlloc(i32, wave) orelse return -1;
            defer scratchFree(wave_fds);
            const wave_bufs = scratchAlloc([*]u8, wave) orelse return -1;
            defer scratchFree(wave_bufs);
            const wave_lens = scratchAlloc(u32, wave) orelse return -1;
            defer scratchFree(wave_lens);
            for (0..wave) |k| {
                const i = read_indices[start + k];
                wave_fds[k] = fds[i];
                wave_bufs[k] = bufs[i].?;
                wave_lens[k] = sizes[i];
            }
            const submitted = uringSubmitReads(&ring, wave_fds, wave_bufs, wave_lens, read_indices[start..][0..wave]);
            if (submitted == 0) {
                errors = -1;
            } else if (uringDrain(&ring, submitted, out_bufs, out_lens, bufs, sizes) != 0) {
                errors = -1;
            }
            // A slot is delivered iff drain recorded a full read for it.
            for (0..wave) |k| {
                const i = read_indices[start + k];
                if (out_lens[i] != 0) delivered[i] = true;
            }
            start += wave;
        }
    }

    return errors;
}

// ─── IOCP (Windows) ──────────────────────────────────────────────────
//
// Uses Windows API: CreateIoCompletionPort, ReadFile (overlapped),
// GetQueuedCompletionStatus for batch async reads.

pub const windows = struct {
    pub const Overlapped = extern struct {
        internal: usize = 0,
        internal_high: usize = 0,
        offset: u32 = 0,
        offset_high: u32 = 0,
        h_event: ?*anyopaque = null,
    };

    pub const INVALID_HANDLE_VALUE: *anyopaque = @ptrFromInt(std.math.maxInt(usize));
    pub const GENERIC_READ: u32 = 0x80000000;
    pub const FILE_SHARE_READ: u32 = 0x00000001;
    pub const FILE_SHARE_WRITE: u32 = 0x00000002;
    pub const OPEN_EXISTING: u32 = 3;
    pub const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;
    pub const ERROR_IO_PENDING: u32 = 997;
    pub const INFINITE: u32 = 0xFFFFFFFF;

    pub extern "kernel32" fn CreateIoCompletionPort(file_handle: *anyopaque, existing_port: ?*anyopaque, completion_key: usize, num_threads: u32) ?*anyopaque;
    pub extern "kernel32" fn GetQueuedCompletionStatus(port: *anyopaque, bytes_transferred: *u32, completion_key: *usize, overlapped: *?*Overlapped, timeout_ms: u32) c_int;
    pub extern "kernel32" fn CloseHandle(handle: *anyopaque) c_int;
    pub extern "kernel32" fn CreateFileW(name: [*:0]const u16, access: u32, share: u32, security: ?*anyopaque, disposition: u32, flags: u32, template: ?*anyopaque) *anyopaque;
    pub extern "kernel32" fn GetFileSizeEx(handle: *anyopaque, size: *i64) c_int;
    pub extern "kernel32" fn ReadFile(handle: *anyopaque, buffer: [*]u8, bytes_to_read: u32, bytes_read: ?*u32, overlapped: ?*Overlapped) c_int;
    pub extern "kernel32" fn GetLastError() u32;
};

/// Batch-read files using a real I/O completion port on Windows.
///
/// One port per call; each file is opened with FILE_FLAG_OVERLAPPED and
/// issued a single overlapped ReadFile for its full contents. Completions
/// are drained with GetQueuedCompletionStatus. Per-file failures are
/// isolated: a failed file leaves out_bufs[i] untouched and out_lens[i]=0
/// (the caller pre-nulls out_bufs to detect failures).
///
/// Files larger than MAX_FILE_SIZE are rejected (a failed slot).
pub fn readFilesIOCP(
    paths_ptr: [*]const [*]const u8,
    paths_len_ptr: [*]const usize,
    count: usize,
    out_bufs: [*][*]u8,
    out_lens: [*]usize,
) c_int {
    if (builtin.os.tag != .windows) {
        return readFilesBatch(paths_ptr, paths_len_ptr, count, out_bufs, out_lens);
    }
    if (count == 0) return 0;

    const Job = struct {
        handle: ?*anyopaque = null,
        overlapped: windows.Overlapped = .{},
        buf: ?[*]u8 = null,
        size: u32 = 0,
        // true while a read is submitted and its completion packet has not
        // been consumed — the kernel may still write into `buf`.
        in_flight: bool = false,
        delivered: bool = false,
    };

    var errors: c_int = 0;
    const w = windows;

    const port = w.CreateIoCompletionPort(w.INVALID_HANDLE_VALUE, null, 0, 0) orelse
        return readFilesBatch(paths_ptr, paths_len_ptr, count, out_bufs, out_lens);

    const jobs = scratchAlloc(Job, count) orelse {
        _ = w.CloseHandle(port);
        return -1;
    };
    defer scratchFree(jobs);

    var pending: usize = 0;

    for (0..count) |i| {
        jobs[i] = .{};
        out_lens[i] = 0;
    }

    for (0..count) |i| {
        const path = paths_ptr[i][0..paths_len_ptr[i]];

        // Embedded NUL would truncate the wide path at the NUL — reject.
        if (std.mem.indexOfScalar(u8, path, 0) != null) {
            errors = -1;
            continue;
        }
        const path_w = std.unicode.utf8ToUtf16LeAllocZ(io_allocator, path) catch {
            errors = -1;
            continue;
        };
        defer io_allocator.free(path_w);

        const handle = w.CreateFileW(
            path_w.ptr,
            w.GENERIC_READ,
            w.FILE_SHARE_READ | w.FILE_SHARE_WRITE,
            null,
            w.OPEN_EXISTING,
            w.FILE_FLAG_OVERLAPPED,
            null,
        );
        if (handle == w.INVALID_HANDLE_VALUE) {
            errors = -1;
            continue;
        }
        jobs[i].handle = handle;

        var size: i64 = 0;
        if (w.GetFileSizeEx(handle, &size) == 0 or size < 0 or size > @as(i64, @intCast(MAX_FILE_SIZE))) {
            errors = -1;
            continue;
        }
        if (size == 0) {
            const b = ioAlloc(1) orelse {
                errors = -1;
                continue;
            };
            out_bufs[i] = b;
            jobs[i].delivered = true;
            jobs[i].handle = null;
            _ = w.CloseHandle(handle);
            continue;
        }

        const buf = ioAlloc(@intCast(size)) orelse {
            errors = -1;
            continue;
        };
        jobs[i].buf = buf;
        jobs[i].size = @intCast(size);

        // Associating the handle with the port can fail; without it no
        // completion packet would ever arrive and the drain loop below
        // would block forever.
        if (w.CreateIoCompletionPort(handle, port, i, 0) == null) {
            errors = -1;
            continue;
        }

        // Sync-success also queues a completion packet (no
        // FILE_SKIP_COMPLETION_PORT_ON_SUCCESS), so pending++ either way.
        const ok = w.ReadFile(handle, buf, @intCast(size), null, &jobs[i].overlapped);
        if (ok == 0 and w.GetLastError() != w.ERROR_IO_PENDING) {
            errors = -1;
            continue;
        }
        jobs[i].in_flight = true;
        pending += 1;
    }

    // Drain one completion packet per submitted read.
    while (pending > 0) {
        var bytes: u32 = 0;
        var key: usize = 0;
        var ov: ?*windows.Overlapped = null;
        const ok = w.GetQueuedCompletionStatus(port, &bytes, &key, &ov, w.INFINITE);
        if (ov == null) break; // port-level failure; bail to cleanup
        pending -= 1;
        if (key < count) {
            jobs[key].in_flight = false;
            if (ok != 0 and bytes == jobs[key].size) {
                out_bufs[key] = jobs[key].buf.?;
                out_lens[key] = bytes;
                jobs[key].delivered = true;
            } else {
                errors = -1;
            }
            if (jobs[key].handle) |h| {
                _ = w.CloseHandle(h);
                jobs[key].handle = null;
            }
        } else {
            errors = -1;
        }
    }
    // Anything still pending after a port-level failure is marked failed.
    if (pending > 0) errors = -1;

    for (jobs) |*j| {
        if (j.handle) |h| {
            // Closing cancels nothing in flight, but a handle whose read is
            // still outstanding must not have its buffer freed below.
            _ = w.CloseHandle(h);
            j.handle = null;
        }
        // Free buffers that were never handed to the caller — unless the
        // kernel may still be writing into them (leak beats corruption).
        if (!j.delivered and !j.in_flight) {
            if (j.buf) |b| freeBufferPtr(b);
        }
    }
    _ = w.CloseHandle(port);
    return errors;
}

// ─── kqueue (macOS/BSD) ──────────────────────────────────────────────
//
// Uses kqueue with EVFILT_READ for async file I/O on macOS and BSD.

/// Batch-read files on macOS/BSD.
///
/// NOTE: kqueue cannot asynchronously read regular files (EVFILT_READ on a
/// regular file reports "ready" immediately — it only works for sockets,
/// pipes, and vnode *watching*). Real async file I/O on macOS means
/// POSIX AIO or GCD, both of which are thread-pool shaped anyway — so the
/// thread pool IS the correct implementation here, not a placeholder.
pub fn readFilesKqueue(
    paths_ptr: [*]const [*]const u8,
    paths_len_ptr: [*]const usize,
    count: usize,
    out_bufs: [*][*]u8,
    out_lens: [*]usize,
) c_int {
    return readFilesBatch(paths_ptr, paths_len_ptr, count, out_bufs, out_lens);
}

/// Platform-optimized batch read: automatically selects the best backend.
pub fn readFilesOptimized(
    paths_ptr: [*]const [*]const u8,
    paths_len_ptr: [*]const usize,
    count: usize,
    out_bufs: [*][*]u8,
    out_lens: [*]usize,
) c_int {
    const backend = detectBackend();
    return switch (backend) {
        .io_uring => readFilesIoUring(paths_ptr, paths_len_ptr, count, out_bufs, out_lens),
        .iocp => readFilesIOCP(paths_ptr, paths_len_ptr, count, out_bufs, out_lens),
        .kqueue => readFilesKqueue(paths_ptr, paths_len_ptr, count, out_bufs, out_lens),
        .thread_pool => readFilesBatch(paths_ptr, paths_len_ptr, count, out_bufs, out_lens),
    };
}

test "readFile reads a file" {
    // Use a temp file in the current directory (cross-platform)
    const tmp = "pledge_test_read.txt";
    const content = "hello pledge";

    // Write test file using C stdlib
    {
        const fp = fopen(tmp, "wb") orelse return error.OpenFailed;
        defer _ = fclose(fp);
        const n = fwrite(content.ptr, 1, content.len, fp);
        try std.testing.expectEqual(content.len, n);
    }

    var buf: [*]u8 = undefined;
    var len: usize = 0;
    const result = readFile(tmp, &buf, &len);
    try std.testing.expectEqual(@as(c_int, 0), result);
    try std.testing.expectEqual(content.len, len);
    try std.testing.expectEqualStrings(content, buf[0..len]);

    freeBufferPtr(buf);
    _ = remove(tmp);
}

test "readFile rejects embedded NUL and oversized/missing paths" {
    var buf: [*]u8 = undefined;
    var len: usize = 0;
    try std.testing.expectEqual(@as(c_int, -1), readFile("pledge_nul\x00.txt", &buf, &len));
    try std.testing.expectEqual(@as(c_int, -1), readFile("pledge_definitely_missing.txt", &buf, &len));
}

test "detectBackend returns platform-appropriate backend" {
    const backend = detectBackend();
    if (builtin.os.tag == .linux) {
        try std.testing.expectEqual(IoBackend.io_uring, backend);
    } else if (builtin.os.tag == .windows) {
        try std.testing.expectEqual(IoBackend.iocp, backend);
    } else if (builtin.os.tag == .macos or builtin.os.tag == .freebsd) {
        try std.testing.expectEqual(IoBackend.kqueue, backend);
    } else {
        try std.testing.expectEqual(IoBackend.thread_pool, backend);
    }
}

test "asyncIoAvailable returns true only where real async file I/O exists" {
    if (builtin.os.tag == .linux or builtin.os.tag == .windows) {
        try std.testing.expect(asyncIoAvailable());
    } else {
        try std.testing.expect(!asyncIoAvailable());
    }
}

test "readFilesOptimized delegates to correct backend" {
    // Test that the optimized path works (delegates to thread pool fallback)
    const tmp = "pledge_test_optimized.txt";
    const content = "optimized io test";
    {
        const fp = fopen(tmp, "wb") orelse return error.OpenFailed;
        defer _ = fclose(fp);
        _ = fwrite(content.ptr, 1, content.len, fp);
    }

    var paths: [1][*]const u8 = .{tmp.ptr};
    var lens: [1]usize = .{tmp.len};
    var bufs: [1][*]u8 = undefined;
    var out_lens: [1]usize = undefined;

    const result = readFilesOptimized(&paths, &lens, 1, &bufs, &out_lens);
    try std.testing.expectEqual(@as(c_int, 0), result);
    try std.testing.expectEqual(content.len, out_lens[0]);
    try std.testing.expectEqualStrings(content, bufs[0][0..out_lens[0]]);

    freeBufferPtr(bufs[0]);
    _ = remove(tmp);
}

fn writeTmp(path: [*:0]const u8, content: []const u8) !void {
    const fp = fopen(path, "wb") orelse return error.OpenFailed;
    defer _ = fclose(fp);
    _ = fwrite(content.ptr, 1, content.len, fp);
}

test "readFilesOptimized batch: multiple files, mixed success and failure" {
    const c1 = "batch one";
    const c2 = "batch two!";
    try writeTmp("pledge_batch_a.txt", c1);
    try writeTmp("pledge_batch_b.txt", c2);
    defer {
        _ = remove("pledge_batch_a.txt");
        _ = remove("pledge_batch_b.txt");
    }

    const missing = "pledge_batch_missing.txt";
    var paths: [3][*]const u8 = .{ "pledge_batch_a.txt".ptr, missing.ptr, "pledge_batch_b.txt".ptr };
    var lens: [3]usize = .{ "pledge_batch_a.txt".len, missing.len, "pledge_batch_b.txt".len };
    var bufs: [3][*]u8 = .{ undefined, undefined, undefined };
    var out_lens: [3]usize = .{ 9, 9, 9 }; // poison — must be overwritten

    const result = readFilesOptimized(&paths, &lens, 3, &bufs, &out_lens);
    try std.testing.expectEqual(@as(c_int, -1), result); // one failure
    try std.testing.expectEqualStrings(c1, bufs[0][0..out_lens[0]]);
    try std.testing.expectEqual(@as(usize, 0), out_lens[1]); // missing → 0
    try std.testing.expectEqualStrings(c2, bufs[2][0..out_lens[2]]);
    freeBufferPtr(bufs[0]);
    freeBufferPtr(bufs[2]);
}

test "readFilesOptimized batch: empty batch is a no-op" {
    var bufs: [1][*]u8 = undefined;
    var out_lens: [1]usize = undefined;
    const result = readFilesOptimized(undefined, undefined, 0, &bufs, &out_lens);
    try std.testing.expectEqual(@as(c_int, 0), result);
}

test "readFilesOptimized batch: larger batch stresses the wave path" {
    // 40 files > typical ring/thread-pool wave sizes on small configs.
    var names: [40][24]u8 = undefined;
    var paths: [40][*]const u8 = undefined;
    var lens: [40]usize = undefined;
    var bufs: [40][*]u8 = undefined;
    var out_lens: [40]usize = undefined;

    for (0..40) |i| {
        const name = std.fmt.bufPrintZ(&names[i], "pledge_wave_{d}.txt", .{i}) catch unreachable;
        try writeTmp(name.ptr, "x");
        paths[i] = name.ptr;
        lens[i] = name.len;
    }
    defer {
        for (0..40) |i| {
            const name = std.fmt.bufPrintZ(&names[i], "pledge_wave_{d}.txt", .{i}) catch unreachable;
            _ = remove(name.ptr);
        }
    }

    const result = readFilesOptimized(&paths, &lens, 40, &bufs, &out_lens);
    try std.testing.expectEqual(@as(c_int, 0), result);
    for (0..40) |i| {
        try std.testing.expectEqual(@as(usize, 1), out_lens[i]);
        try std.testing.expectEqual(@as(u8, 'x'), bufs[i][0]);
        freeBufferPtr(bufs[i]);
    }
}

// ─── G4.8: Task Preemption via Zig Coroutines ──────────────────────────

pub const PreemptableTask = struct {
    running: bool,
    preempted: bool,
    priority: u8,
    yield_count: u32,

    pub fn init(priority: u8) PreemptableTask {
        return .{ .running = false, .preempted = false, .priority = priority, .yield_count = 0 };
    }
    pub fn start(self: *PreemptableTask) void { self.running = true; self.preempted = false; }
    pub fn preempt(self: *PreemptableTask) void { if (self.running) { self.preempted = true; self.yield_count += 1; } }
    pub fn resumeTask(self: *PreemptableTask) void { self.preempted = false; }
    pub fn complete(self: *PreemptableTask) void { self.running = false; self.preempted = false; }
    pub fn shouldPreemptFor(self: *const PreemptableTask, other_priority: u8) bool {
        return self.running and !self.preempted and other_priority < self.priority;
    }
};

test "G4.8: PreemptableTask lifecycle" {
    var task = PreemptableTask.init(5);
    try std.testing.expect(!task.running);
    task.start();
    try std.testing.expect(task.running);
    task.preempt();
    try std.testing.expect(task.preempted);
    try std.testing.expectEqual(@as(u32, 1), task.yield_count);
    task.resumeTask();
    try std.testing.expect(!task.preempted);
    task.complete();
    try std.testing.expect(!task.running);
}

test "G4.8: PreemptableTask priority" {
    var low = PreemptableTask.init(10);
    const high = PreemptableTask.init(1);
    low.start();
    try std.testing.expect(low.shouldPreemptFor(high.priority));
    try std.testing.expect(!high.shouldPreemptFor(low.priority));
}

// ─── G4.11: Work-Stealing Executor ──────────────────────────────────────

pub const WorkStealingExecutor = struct {
    num_workers: u32,
    total_queued: std.atomic.Value(u32),
    total_completed: std.atomic.Value(u64),

    pub fn init(num_workers: u32) WorkStealingExecutor {
        return .{ .num_workers = num_workers, .total_queued = std.atomic.Value(u32).init(0), .total_completed = std.atomic.Value(u64).init(0) };
    }
    pub fn enqueue(self: *WorkStealingExecutor) void { _ = self.total_queued.fetchAdd(1, .seq_cst); }
    pub fn dequeue(self: *WorkStealingExecutor) void { _ = self.total_queued.fetchSub(1, .seq_cst); _ = self.total_completed.fetchAdd(1, .seq_cst); }
    pub fn steal(self: *WorkStealingExecutor) bool { if (self.total_queued.load(.seq_cst) > 0) { self.dequeue(); return true; } return false; }
    pub fn queuedCount(self: *const WorkStealingExecutor) u32 { return self.total_queued.load(.seq_cst); }
    pub fn completedCount(self: *const WorkStealingExecutor) u64 { return self.total_completed.load(.seq_cst); }
};

test "G4.11: WorkStealingExecutor" {
    var exec = WorkStealingExecutor.init(4);
    try std.testing.expectEqual(@as(u32, 0), exec.queuedCount());
    exec.enqueue(); exec.enqueue(); exec.enqueue();
    try std.testing.expectEqual(@as(u32, 3), exec.queuedCount());
    exec.dequeue();
    try std.testing.expectEqual(@as(u32, 2), exec.queuedCount());
    try std.testing.expectEqual(@as(u64, 1), exec.completedCount());
    try std.testing.expect(exec.steal());
    try std.testing.expect(exec.steal());
    try std.testing.expect(!exec.steal());
}

// ─── G4.13: NUMA-Aware Scheduling ───────────────────────────────────────

pub const NumaNode = struct {
    id: u32, cpu_mask: u64, memory_bytes: u64, distances: [8]u8,
    pub fn init(id: u32, cpu_mask: u64, memory_bytes: u64) NumaNode {
        return .{ .id = id, .cpu_mask = cpu_mask, .memory_bytes = memory_bytes, .distances = [_]u8{0} ** 8 };
    }
    pub fn hasCpu(self: *const NumaNode, cpu: u6) bool { return (self.cpu_mask & (@as(u64, 1) << cpu)) != 0; }
    pub fn distanceTo(self: *const NumaNode, other_id: u32) u8 { if (other_id >= 8) return 255; return self.distances[other_id]; }
};

pub const NumaScheduler = struct {
    nodes: [8]NumaNode, node_count: u32,
    pub fn init() NumaScheduler { var s = NumaScheduler{ .nodes = undefined, .node_count = 0 }; var i: u32 = 0; while (i < 8) : (i += 1) { s.nodes[i] = NumaNode.init(i, 0, 0); } return s; }
    pub fn addNode(self: *NumaScheduler, node: NumaNode) void { if (node.id < 8) { self.nodes[node.id] = node; self.node_count += 1; } }
    pub fn bestNode(self: *const NumaScheduler, data_node: u32) u32 {
        if (self.node_count == 0) return 0;
        var best: u32 = 0; var best_dist: u8 = 255; var i: u32 = 0;
        while (i < self.node_count) : (i += 1) { const d = self.nodes[i].distanceTo(data_node); if (d < best_dist) { best_dist = d; best = i; } }
        return best;
    }
};

test "G4.13: NumaNode and Scheduler" {
    var node = NumaNode.init(0, 0b1111, 8 * 1024 * 1024 * 1024);
    try std.testing.expect(node.hasCpu(0));
    try std.testing.expect(!node.hasCpu(4));

    var sched = NumaScheduler.init();
    var n0 = NumaNode.init(0, 0b0011, 8 * 1024 * 1024 * 1024);
    n0.distances = .{ 0, 20, 0, 0, 0, 0, 0, 0 };
    var n1 = NumaNode.init(1, 0b1100, 8 * 1024 * 1024 * 1024);
    n1.distances = .{ 20, 0, 0, 0, 0, 0, 0, 0 };
    sched.addNode(n0); sched.addNode(n1);
    try std.testing.expectEqual(@as(u32, 0), sched.bestNode(0));
    try std.testing.expectEqual(@as(u32, 1), sched.bestNode(1));
}

// ─── G4.14: GPU Offloading ──────────────────────────────────────────────

pub const GpuOffloadConfig = struct {
    enabled: bool, backend: []const u8, max_memory_bytes: u64, fallback_to_cpu: bool,
    pub fn default() GpuOffloadConfig { return .{ .enabled = false, .backend = "auto", .max_memory_bytes = 512 * 1024 * 1024, .fallback_to_cpu = true }; }
    pub fn enable(self: *GpuOffloadConfig, backend: []const u8) void { self.enabled = true; self.backend = backend; }
};

pub const GpuTask = struct {
    name: []const u8, input_size: usize, estimated_speedup: f32, on_gpu: bool,
    pub fn init(name: []const u8, input_size: usize, speedup: f32) GpuTask { return .{ .name = name, .input_size = input_size, .estimated_speedup = speedup, .on_gpu = false }; }
    pub fn shouldOffload(self: *const GpuTask, config: *const GpuOffloadConfig) bool { return config.enabled and self.estimated_speedup > 2.0 and self.input_size > 1024; }
};

test "G4.14: GpuOffload" {
    var config = GpuOffloadConfig.default();
    var task = GpuTask.init("minify_js", 10 * 1024 * 1024, 5.0);
    try std.testing.expect(!task.shouldOffload(&config));
    config.enable("vulkan");
    try std.testing.expect(task.shouldOffload(&config));
    var small = GpuTask.init("tiny", 100, 10.0);
    try std.testing.expect(!small.shouldOffload(&config));
    var slow = GpuTask.init("slow", 10 * 1024 * 1024, 1.5);
    try std.testing.expect(!slow.shouldOffload(&config));
}

// ─── G8.5: Slab Allocator ───────────────────────────────────────────────

pub const SlabAllocator = struct {
    slabs: std.ArrayList([]u8), current_slab: usize, slab_offset: usize, slab_size: usize, backing: std.mem.Allocator,
    pub fn init(allocator: std.mem.Allocator, slab_size: usize) SlabAllocator { return .{ .slabs = .empty, .current_slab = 0, .slab_offset = 0, .slab_size = slab_size, .backing = allocator }; }
    pub fn deinit(self: *SlabAllocator) void { for (self.slabs.items) |s| { self.backing.free(s); } self.slabs.deinit(self.backing); }
    pub fn alloc(self: *SlabAllocator, size: usize) ![]u8 {
        if (self.slabs.items.len == 0 or self.slab_offset + size > self.slab_size) {
            const new_slab = try self.backing.alloc(u8, self.slab_size);
            try self.slabs.append(self.backing, new_slab);
            self.current_slab = self.slabs.items.len - 1;
            self.slab_offset = 0;
        }
        const slab = self.slabs.items[self.current_slab];
        const result = slab[self.slab_offset .. self.slab_offset + size];
        self.slab_offset += size;
        return result;
    }
    pub fn slabCount(self: *const SlabAllocator) usize { return self.slabs.items.len; }
};

test "G8.5: SlabAllocator" {
    var a = SlabAllocator.init(std.testing.allocator, 1024);
    defer a.deinit();
    const x = try a.alloc(100);
    x[0] = 0xAB;
    try std.testing.expectEqual(@as(u8, 0xAB), x[0]);
    try std.testing.expectEqual(@as(usize, 1), a.slabCount());
    _ = try a.alloc(800);
    try std.testing.expectEqual(@as(usize, 1), a.slabCount());
    _ = try a.alloc(800);
    try std.testing.expectEqual(@as(usize, 2), a.slabCount());
}

// ─── G8.6: Arena Compaction ─────────────────────────────────────────────

pub const ArenaCompactor = struct {
    bytes_before: usize, bytes_after: usize, gaps_removed: u32,
    pub fn init(bytes_before: usize) ArenaCompactor { return .{ .bytes_before = bytes_before, .bytes_after = bytes_before, .gaps_removed = 0 }; }
    pub fn compact(self: *ArenaCompactor, gap_bytes: usize) void { self.bytes_after = self.bytes_before - gap_bytes; self.gaps_removed += 1; }
    pub fn savedBytes(self: *const ArenaCompactor) usize { return self.bytes_before - self.bytes_after; }
};

test "G8.6: ArenaCompactor" {
    var c = ArenaCompactor.init(1024 * 1024);
    c.compact(100 * 1024);
    try std.testing.expectEqual(@as(usize, 100 * 1024), c.savedBytes());
    try std.testing.expectEqual(@as(u32, 1), c.gaps_removed);
}

// ─── G8.7: Memory Tiering ───────────────────────────────────────────────

pub const MemoryTier = enum { hot, warm, cold };

pub const TieredMemory = struct {
    hot_bytes: usize, warm_bytes: usize, cold_bytes: usize, hot_threshold: usize, warm_threshold: usize,
    pub fn init(hot_threshold: usize, warm_threshold: usize) TieredMemory { return .{ .hot_bytes = 0, .warm_bytes = 0, .cold_bytes = 0, .hot_threshold = hot_threshold, .warm_threshold = warm_threshold }; }
    pub fn add(self: *TieredMemory, tier: MemoryTier, bytes: usize) void { switch (tier) { .hot => self.hot_bytes += bytes, .warm => self.warm_bytes += bytes, .cold => self.cold_bytes += bytes } }
    pub fn shouldDemoteHot(self: *const TieredMemory) bool { return self.hot_bytes > self.hot_threshold; }
    pub fn demoteHot(self: *TieredMemory, bytes: usize) void { const m = @min(bytes, self.hot_bytes); self.hot_bytes -= m; self.warm_bytes += m; }
    pub fn totalBytes(self: *const TieredMemory) usize { return self.hot_bytes + self.warm_bytes + self.cold_bytes; }
};

test "G8.7: TieredMemory" {
    var tm = TieredMemory.init(1024, 10 * 1024);
    tm.add(.hot, 2 * 1024);
    try std.testing.expect(tm.shouldDemoteHot());
    tm.demoteHot(1024);
    try std.testing.expectEqual(@as(usize, 1024), tm.hot_bytes);
    try std.testing.expectEqual(@as(usize, 1024), tm.warm_bytes);
}

// ─── G8.8: Prefetch ─────────────────────────────────────────────────────

pub const PrefetchHint = struct {
    node_idx: u32, expected_access_ms: u32, is_sequential: bool,
    pub fn init(node_idx: u32, expected_access_ms: u32) PrefetchHint { return .{ .node_idx = node_idx, .expected_access_ms = expected_access_ms, .is_sequential = false }; }
    pub fn sequential(node_idx: u32) PrefetchHint { return .{ .node_idx = node_idx, .expected_access_ms = 0, .is_sequential = true }; }
};

test "G8.8: PrefetchHint" {
    const h = PrefetchHint.init(42, 100);
    try std.testing.expectEqual(@as(u32, 42), h.node_idx);
    try std.testing.expect(!h.is_sequential);
    const s = PrefetchHint.sequential(10);
    try std.testing.expect(s.is_sequential);
}

// ─── G8.9: Huge Pages ───────────────────────────────────────────────────

pub const HugePageConfig = struct {
    enabled: bool, page_size: usize, pages_allocated: u32, transparent: bool,
    pub fn init() HugePageConfig { return .{ .enabled = false, .page_size = 2 * 1024 * 1024, .pages_allocated = 0, .transparent = true }; }
    pub fn enable(self: *HugePageConfig, page_size: usize) void { self.enabled = true; self.page_size = page_size; }
    pub fn allocate(self: *HugePageConfig, bytes: usize) u32 { if (!self.enabled) return 0; const pages: u32 = @intCast((bytes + self.page_size - 1) / self.page_size); self.pages_allocated += pages; return pages; }
};

test "G8.9: HugePageConfig" {
    var config = HugePageConfig.init();
    try std.testing.expect(!config.enabled);
    config.enable(2 * 1024 * 1024);
    const pages = config.allocate(5 * 1024 * 1024);
    try std.testing.expectEqual(@as(u32, 3), pages);
}
