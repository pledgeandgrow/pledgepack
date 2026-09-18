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
    return io.readFilesBatch(
        paths_ptr,
        paths_len_ptr,
        count,
        out_bufs,
        out_lens,
    );
}

export fn pledge_io_free(buf: [*]u8, len: usize) callconv(.c) void {
    io.freeBuffer(buf[0..len]);
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

test "library loads" {
    std.testing.refAllDecls(@This());
}
