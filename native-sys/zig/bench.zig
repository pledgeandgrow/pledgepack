// Benchmark suite for pledge-native
// Run with: zig build bench

const std = @import("std");
const io = @import("io.zig");
const graph = @import("graph.zig");
const simd = @import("simd.zig");

// C stdlib for file writing in benchmarks
extern "c" fn fopen(path: [*:0]const u8, mode: [*:0]const u8) ?*anyopaque;
extern "c" fn fclose(stream: *anyopaque) c_int;
extern "c" fn fwrite(ptr: [*]const u8, size: usize, nmemb: usize, stream: *anyopaque) usize;
extern "c" fn remove(path: [*:0]const u8) c_int;

pub fn main(init: std.process.Init) !void {
    const stdout = std.Io.File.stdout().writer(&.{});
    const w = &stdout.interface;

    try w.print("\n", .{});
    try w.print("=== pledge-native benchmark suite ===\n\n", .{});

    // ─── Graph benchmark ───
    try w.print("-- Module Graph --\n", .{});
    try benchGraph(w);

    // ─── SIMD benchmark ───
    try w.print("\n-- SIMD Source Scanning --\n", .{});
    try benchSimd(w);

    // ─── I/O benchmark ───
    try w.print("\n-- File I/O --\n", .{});
    try benchIo(w, init.gpa);
}

fn benchGraph(writer: anytype) !void {
    var g = graph.ModuleGraph.init();
    defer g.deinit();

    // Build a graph with 10,000 modules
    const count = 10_000;
    const allocator = std.heap.page_allocator;

    var timer = try std.time.Timer.start();
    for (0..count) |i| {
        var path_buf: [64]u8 = undefined;
        const path = try std.fmt.bufPrint(&path_buf, "src/module_{d}.ts", .{i});
        _ = try g.addModule(path);
    }
    const add_time = timer.lap();
    try writer.print("  Add {d} modules:        {d}ms\n", .{ count, add_time / std.time.ns_per_ms });

    // Add dependencies (each module imports 3 others)
    timer.reset();
    for (0..count) |i| {
        const from: u32 = @intCast(i);
        const dep1: u32 = @intCast((i + 1) % count);
        const dep2: u32 = @intCast((i + 2) % count);
        const dep3: u32 = @intCast((i + 3) % count);
        try g.addDependency(from, dep1);
        try g.addDependency(from, dep2);
        try g.addDependency(from, dep3);
    }
    const dep_time = timer.lap();
    try writer.print("  Add {d} dependencies:   {d}ms\n", .{ count * 3, dep_time / std.time.ns_per_ms });

    // Traverse dependents (cache locality test)
    timer.reset();
    var total: usize = 0;
    for (0..count) |i| {
        var buf: [64]u32 = undefined;
        total += g.getDependents(@intCast(i), &buf);
    }
    const traverse_time = timer.lap();
    try writer.print("  Traverse dependents:    {d}ms (found {d} edges)\n", .{
        traverse_time / std.time.ns_per_ms,
        total,
    });

    // Invalidation set (BFS)
    timer.reset();
    const invalid = try g.getInvalidationSet(0, allocator);
    defer allocator.free(invalid);
    const invalid_time = timer.lap();
    try writer.print("  Invalidation set (BFS): {d}ms (invalidated {d} modules)\n", .{
        invalid_time / std.time.ns_per_ms,
        invalid.len,
    });

    // Memory usage
    const mem = g.modules.items.len * @sizeOf(graph.Module) +
        g.edges.items.len * @sizeOf(u32) +
        g.reverse_edges.items.len * @sizeOf(u32) +
        g.path_storage.items.len;
    try writer.print("  Memory:                 {d}KB ({d} bytes/module)\n", .{
        mem / 1024,
        mem / count,
    });
}

fn benchSimd(writer: anytype) !void {
    // Generate a large source file with imports scattered throughout
    const size = 1_000_000; // 1MB
    var source = std.heap.page_allocator.alloc(u8, size) catch return;
    defer std.heap.page_allocator.free(source);

    // Fill with code-like content
    var i: usize = 0;
    while (i < size) {
        // Every ~1000 bytes, add an import statement
        if (i % 1000 == 0 and i + 30 < size) {
            const stmt = "import { thing } from './module';\n";
            @memcpy(source[i .. i + stmt.len], stmt);
            i += stmt.len;
        } else {
            source[i] = "const x = 1; // filler code here\n"[i % 30];
            i += 1;
        }
    }

    var offsets: [1024]usize = undefined;

    var timer = try std.time.Timer.start();
    const count = simd.findImports(source, &offsets);
    const elapsed = timer.lap();

    try writer.print("  Scan 1MB source:        {d}us (found {d} imports)\n", .{
        elapsed / std.time.ns_per_us,
        count,
    });
}

fn benchIo(writer: anytype, allocator: std.mem.Allocator) !void {
    const file_count = 100;
    const file_size = 4096;

    // Get cwd via Windows ntdll
    var cwd_buf: [4096]u8 = undefined;
    const cwd_len = std.os.windows.ntdll.RtlGetCurrentDirectory_U(
        cwd_buf.len * 2,
        @ptrCast(&cwd_buf),
    );
    if (cwd_len == 0) return;
    const cwd = cwd_buf[0 .. cwd_len / 2];

    var paths = allocator.alloc([]u8, file_count) catch return;
    defer {
        for (paths) |p| allocator.free(p);
        allocator.free(paths);
    }

    // Create temp files using C stdlib
    for (0..file_count) |i| {
        paths[i] = std.fmt.allocPrintZ(allocator, "{s}\\pledge_bench_{d}.txt", .{ cwd, i }) catch return;

        const fp = fopen(paths[i].ptr, "wb") orelse continue;
        defer _ = fclose(fp);

        var content = allocator.alloc(u8, file_size) catch return;
        defer allocator.free(content);
        @memset(content, 'x');
        _ = fwrite(content.ptr, 1, file_size, fp);
    }
    defer {
        for (paths) |p| _ = remove(p.ptr);
    }

    // Benchmark batch read
    var bufs: [100][*]u8 = undefined;
    var lens: [100]usize = undefined;

    var path_ptrs: [100][*]const u8 = undefined;
    var path_lens: [100]usize = undefined;
    for (0..file_count) |i| {
        path_ptrs[i] = paths[i].ptr;
        path_lens[i] = paths[i].len;
    }

    var timer = try std.time.Timer.start();
    const result = io.readFilesBatch(&path_ptrs, &path_lens, file_count, &bufs, &lens);
    const elapsed = timer.lap();

    if (result == 0) {
        try writer.print("  Batch read {d} files:    {d}ms ({d}KB total)\n", .{
            file_count,
            elapsed / std.time.ns_per_ms,
            (file_size * file_count) / 1024,
        });
    } else {
        try writer.print("  Batch read: FAILED\n", .{});
    }

    if (result == 0) {
        for (0..file_count) |i| io.freeBufferPtr(bufs[i]);
    }
}
