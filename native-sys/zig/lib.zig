// pledge-native: Zig core library
// Exports C ABI functions for Rust FFI
//
// Three subsystems:
//   1. io.zig       — io_uring/kqueue/IOCP file I/O
//   2. graph.zig    — Arena-allocated module dependency graph
//   3. simd.zig     — SIMD-accelerated source scanning

const std = @import("std");

pub const io = @import("io.zig");
pub const graph = @import("graph.zig");
pub const simd = @import("simd.zig");

// Zig 0.16.0's default panic handler pulls in std.debug.SelfInfo (for
// printing a stack trace on panic), and SelfInfo's Windows implementation
// has a compile error for aarch64-windows-msvc specifically
// (`@ptrCast increases pointer alignment` at
// lib/std/debug/SelfInfo/Windows.zig:670, in the LdrRegisterDllNotification
// callback cast — a bug in Zig's own stdlib, not this file). This is a
// static library called via C ABI from Rust; a panic here always indicates
// a bug worth stopping on hard, and Rust-side backtraces (RUST_BACKTRACE=1)
// already cover the FFI boundary, so a minimal, trace-free panic handler
// is sufficient and sidesteps the whole SelfInfo compile path.
pub const panic = std.debug.simple_panic;

// ─── C ABI exports ───

// Graph operations
export fn pledge_graph_create() callconv(.c) *graph.ModuleGraph {
    return graph.create() catch @panic("failed to allocate graph");
}

export fn pledge_graph_destroy(g: *graph.ModuleGraph) callconv(.c) void {
    graph.destroy(g);
}

export fn pledge_graph_add_module(
    g: *graph.ModuleGraph,
    path_ptr: [*]const u8,
    path_len: usize,
) callconv(.c) u32 {
    const path = path_ptr[0..path_len];
    return g.addModule(path) catch @panic("failed to add module");
}

export fn pledge_graph_add_dependency(
    g: *graph.ModuleGraph,
    from: u32,
    to: u32,
) callconv(.c) void {
    g.addDependency(from, to) catch @panic("failed to add dependency");
}

export fn pledge_graph_get_dependents(
    g: *graph.ModuleGraph,
    module_id: u32,
    out_ids: [*]u32,
    out_capacity: usize,
) callconv(.c) usize {
    return g.getDependents(module_id, out_ids[0..out_capacity]);
}

export fn pledge_graph_get_dependencies(
    g: *graph.ModuleGraph,
    module_id: u32,
    out_ids: [*]u32,
    out_capacity: usize,
) callconv(.c) usize {
    const deps = g.getDependencies(module_id);
    const count = @min(deps.len, out_capacity);
    @memcpy(out_ids[0..count], deps[0..count]);
    return count;
}

// ─── Task graph operations ───
// Content-addressed task dependency graph (16-byte blake3 TaskIds).
// TaskIds cross the ABI as *const [16]u8 / [*][16]u8 — plain byte arrays,
// no shared ownership.

export fn pledge_task_graph_create() callconv(.c) *graph.TaskGraph {
    return graph.createTaskGraph() catch @panic("failed to allocate task graph");
}

export fn pledge_task_graph_destroy(g: *graph.TaskGraph) callconv(.c) void {
    graph.destroyTaskGraph(g);
}

export fn pledge_task_graph_add_task(
    g: *graph.TaskGraph,
    id_ptr: *const [16]u8,
) callconv(.c) void {
    _ = g.addTask(id_ptr.*) catch @panic("failed to add task");
}

// Adds both endpoint tasks if absent, then the edge parent→child.
export fn pledge_task_graph_add_edge(
    g: *graph.TaskGraph,
    parent_ptr: *const [16]u8,
    child_ptr: *const [16]u8,
) callconv(.c) void {
    _ = g.addTask(parent_ptr.*) catch @panic("failed to add task");
    _ = g.addTask(child_ptr.*) catch @panic("failed to add task");
    g.addDependency(parent_ptr.*, child_ptr.*) catch |e| switch (e) {
        // Per-task edge counts are packed into 15/14-bit fields. Exceeding
        // them used to wrap silently and corrupt the graph; now it stops hard
        // with an explicit message. Prefer pledge_task_graph_try_add_edge,
        // which reports the condition to the caller instead of aborting.
        error.TooManyDependencies => @panic("task graph: a task has more than 32767 dependencies"),
        error.TooManyDependents => @panic("task graph: a task has more than 16383 dependents"),
        else => @panic("failed to add edge"),
    };
}

// Non-aborting variant of pledge_task_graph_add_edge.
// Returns 0 on success, -1 on allocation failure, -2 if `parent` already has
// the maximum number of dependencies, -3 if `child` already has the maximum
// number of dependents. A rejected edge leaves the graph unchanged.
export fn pledge_task_graph_try_add_edge(
    g: *graph.TaskGraph,
    parent_ptr: *const [16]u8,
    child_ptr: *const [16]u8,
) callconv(.c) c_int {
    _ = g.addTask(parent_ptr.*) catch return -1;
    _ = g.addTask(child_ptr.*) catch return -1;
    g.addDependency(parent_ptr.*, child_ptr.*) catch |e| switch (e) {
        error.TooManyDependencies => return -2,
        error.TooManyDependents => return -3,
        else => return -1,
    };
    return 0;
}

export fn pledge_task_graph_get_dependents(
    g: *graph.TaskGraph,
    id_ptr: *const [16]u8,
    out_ids: [*][16]u8,
    out_capacity: usize,
) callconv(.c) usize {
    return g.getDependents(id_ptr.*, out_ids[0..out_capacity]);
}

export fn pledge_task_graph_get_dependencies(
    g: *graph.TaskGraph,
    id_ptr: *const [16]u8,
    out_ids: [*][16]u8,
    out_capacity: usize,
) callconv(.c) usize {
    return g.getDependencies(id_ptr.*, out_ids[0..out_capacity]);
}

export fn pledge_task_graph_set_status(
    g: *graph.TaskGraph,
    id_ptr: *const [16]u8,
    status: u8,
) callconv(.c) void {
    const idx = g.getIndex(id_ptr.*) orelse return;
    const s: graph.TaskStatus = @enumFromInt(@min(status, 5));
    g.setStatus(idx, s);
}

export fn pledge_task_graph_get_status(
    g: *graph.TaskGraph,
    id_ptr: *const [16]u8,
) callconv(.c) u8 {
    const idx = g.getIndex(id_ptr.*) orelse return @intFromEnum(graph.TaskStatus.pending);
    return @intFromEnum(g.getStatus(idx));
}

export fn pledge_task_graph_count(g: *graph.TaskGraph) callconv(.c) usize {
    return g.taskCount();
}

// Marks the task and all transitive dependents dirty in one pass.
// Returns the number of dirtied ids written (retry with a bigger buffer
// if it equals out_capacity — dirtying is idempotent).
export fn pledge_task_graph_mark_dirty(
    g: *graph.TaskGraph,
    id_ptr: *const [16]u8,
    out_ids: [*][16]u8,
    out_capacity: usize,
) callconv(.c) usize {
    return g.markDirty(id_ptr.*, out_ids[0..out_capacity]);
}

// Flat status scan — dirty_tasks/clean_tasks in one pass over the nodes.
export fn pledge_task_graph_ids_by_status(
    g: *graph.TaskGraph,
    status: u8,
    out_ids: [*][16]u8,
    out_capacity: usize,
) callconv(.c) usize {
    const s: graph.TaskStatus = @enumFromInt(@min(status, 5));
    return g.idsByStatus(s, out_ids[0..out_capacity]);
}

export fn pledge_task_graph_all_ids(
    g: *graph.TaskGraph,
    out_ids: [*][16]u8,
    out_capacity: usize,
) callconv(.c) usize {
    return g.allIds(out_ids[0..out_capacity]);
}

export fn pledge_task_graph_clear(g: *graph.TaskGraph) callconv(.c) void {
    g.clear();
}

// I/O operations
export fn pledge_io_read_file(
    path_ptr: [*]const u8,
    path_len: usize,
    out_buf: *[*]u8,
    out_len: *usize,
) callconv(.c) c_int {
    const path = path_ptr[0..path_len];
    return io.readFile(path, out_buf, out_len);
}

export fn pledge_io_read_files_batch(
    paths_ptr: [*]const [*]const u8,
    paths_len_ptr: [*]const usize,
    count: usize,
    out_bufs: [*][*]u8,
    out_lens: [*]usize,
) callconv(.c) c_int {
    return io.readFilesOptimized(
        paths_ptr,
        paths_len_ptr,
        count,
        out_bufs,
        out_lens,
    );
}

// Releases a buffer returned by pledge_io_read_file / pledge_io_read_files_batch.
// `len` is accepted for ABI stability but ignored: the allocation size is
// recorded in the buffer's own header. Null is a no-op.
export fn pledge_io_free(buf: ?[*]u8, len: usize) callconv(.c) void {
    _ = len;
    if (buf) |b| io.freeBufferPtr(b);
}

// SIMD scanning
export fn pledge_simd_find_imports(
    source_ptr: [*]const u8,
    source_len: usize,
    out_offsets: [*]usize,
    out_capacity: usize,
) callconv(.c) usize {
    const source = source_ptr[0..source_len];
    return simd.findImports(source, out_offsets[0..out_capacity]);
}

// One-pass module summary: offsets + classified counts + flags + hash.
export fn pledge_simd_summarize_module(
    source_ptr: [*]const u8,
    source_len: usize,
    out_summary: *simd.ModuleSummary,
    out_offsets: [*]usize,
    out_capacity: usize,
) callconv(.c) usize {
    const source = source_ptr[0..source_len];
    return simd.summarizeModule(source, out_summary, out_offsets[0..out_capacity]);
}

test "library loads" {
    std.testing.refAllDecls(@This());
}
