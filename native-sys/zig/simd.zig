// SIMD-accelerated source scanning
// Finds import/export statements using vectorized string matching
//
// On x86_64: uses @Vector(32, u8) for 256-bit SIMD (AVX2)
// On aarch64: uses @Vector(16, u8) for 128-bit NEON
// Falls back to scalar on unsupported platforms

const std = @import("std");
const builtin = @import("builtin");

/// Find all occurrences of `import` keyword in source code.
/// Returns offsets into the source where `import` appears.
/// Returns the number of matches found (capped at out_capacity).
pub fn findImports(source: []const u8, out_offsets: []usize) usize {
    return findPattern(source, "import", out_offsets);
}

/// Find all occurrences of `export` keyword in source code.
pub fn findExports(source: []const u8, out_offsets: []usize) usize {
    return findPattern(source, "export", out_offsets);
}

/// Find all occurrences of `require(` in source code.
pub fn findRequires(source: []const u8, out_offsets: []usize) usize {
    return findPattern(source, "require(", out_offsets);
}

/// Generic SIMD pattern matcher.
/// Uses 32-byte wide comparisons on x86_64, 16-byte on other platforms.
pub fn findPattern(source: []const u8, pattern: []const u8, out_offsets: []usize) usize {
    if (pattern.len == 0 or source.len < pattern.len) return 0;

    var count: usize = 0;
    const simd_width: usize = if (builtin.cpu.arch == .x86_64) 32 else 16;

    var i: usize = 0;
    const end = source.len - pattern.len + 1;

    // Process in SIMD-width chunks
    while (i + simd_width < end) {
        // Check if any byte in this chunk matches the first byte of pattern
        const chunk = source[i .. i + simd_width];
        const first_byte = pattern[0];

        // Vectorized comparison: check each byte against first_byte
        const matches = findByteInChunk(chunk, first_byte);

        // For each match position, verify the full pattern
        for (matches) |pos| {
            if (pos == std.math.maxInt(usize)) continue; // sentinel, no more matches
            const abs_pos = i + pos;
            if (abs_pos + pattern.len <= source.len) {
                if (std.mem.eql(u8, source[abs_pos .. abs_pos + pattern.len], pattern)) {
                    if (count < out_offsets.len) {
                        out_offsets[count] = abs_pos;
                        count += 1;
                    }
                }
            }
        }

        i += simd_width;
    }

    // Scalar fallback for remaining bytes
    while (i < end) : (i += 1) {
        if (source[i] == pattern[0] and
            i + pattern.len <= source.len and
            std.mem.eql(u8, source[i .. i + pattern.len], pattern))
        {
            if (count < out_offsets.len) {
                out_offsets[count] = i;
                count += 1;
            }
        }
    }

    return count;
}

/// Find all positions in a chunk where byte == target.
/// Returns an array of relative positions (0-based within the chunk).
fn findByteInChunk(chunk: []const u8, target: u8) [32]usize {
    var positions: [32]usize = undefined;
    var count: usize = 0;

    if (builtin.cpu.arch == .x86_64 and chunk.len >= 32) {
        // 256-bit SIMD: load 32 bytes, compare against target
        const vec: @Vector(32, u8) = chunk[0..32].*;
        const target_vec: @Vector(32, u8) = @splat(target);

        // Compare and convert to mask: use @select to get 0/1 bytes, then pack
        const cmp = vec == target_vec;
        const ones: @Vector(32, u8) = @splat(1);
        const zeros: @Vector(32, u8) = @splat(0);
        const mask_bytes: @Vector(32, u8) = @select(u8, cmp, ones, zeros);

        // Extract match positions by checking each byte
        inline for (0..32) |i| {
            if (mask_bytes[i] == 1 and count < 32) {
                positions[count] = i;
                count += 1;
            }
        }
    } else {
        // Scalar fallback
        for (chunk, 0..) |b, pos| {
            if (b == target) {
                if (count < 32) {
                    positions[count] = pos;
                    count += 1;
                }
            }
        }
    }

    // Zero out remaining positions — use saturating arithmetic to avoid overflow
    while (count < 32) {
        positions[count] = std.math.maxInt(usize);
        count += 1;
    }

    return positions;
}

test "findImports finds import statements" {
    const source =
        \\import React from 'react';
        \\import { useState } from 'react';
        \\const x = 1;
        \\export default function App() {
        \\  import('./lazy').then(m => m.default());
        \\}
    ;

    var offsets: [16]usize = undefined;
    const count = findImports(source, &offsets);
    try std.testing.expect(count >= 2);
    try std.testing.expectEqualStrings("import", source[offsets[0] .. offsets[0] + 6]);
}

test "findExports finds export statements" {
    const source =
        \\export const foo = 1;
        \\export default App;
        \\export { bar };
    ;

    var offsets: [16]usize = undefined;
    const count = findExports(source, &offsets);
    try std.testing.expectEqual(@as(usize, 3), count);
}

test "findRequires finds require calls" {
    const source =
        \\const fs = require('fs');
        \\const path = require('path');
    ;

    var offsets: [16]usize = undefined;
    const count = findRequires(source, &offsets);
    try std.testing.expectEqual(@as(usize, 2), count);
}

// ─── One-pass module summary ─────────────────────────────────────────
//
// `summarizeModule` makes a single memory-bandwidth sweep over a source
// file and produces everything graph construction needs BEFORE the AST
// parse: import/re-export offsets (for specifier extraction), classified
// counts (static/dynamic/require/export), RSC directive flags, a CJS
// signal, and a 128-bit content hash — all fused into one pass instead of
// separate findImports/findExports/findRequires/hash sweeps.
//
// Recorded offsets are keyword *candidates* — the Rust-side
// `extract_module_specifier` validator applies statement-boundary and
// `from`-clause rules, identical to the findImports path. Two differences
// vs findImports, both strict improvements:
//   • `export ... from` re-export candidates are emitted (re-exports were
//     never discovered as dependencies by findImports)
//   • `import.meta` and ident-suffix false hits (`important`) are dropped
//     here — the Rust validator would reject them anyway, so the dep set
//     is unchanged, just with less wasted work.

/// Bit flags in ModuleSummary.flags.
pub const SUMMARY_USE_CLIENT: u32 = 1 << 0;
pub const SUMMARY_USE_SERVER: u32 = 1 << 1;
/// `require()` call or `module.exports`/`exports.x` seen — CJS interop hint.
pub const SUMMARY_USES_COMMONJS: u32 = 1 << 2;
/// `console.`/`process.env`/`window`/`document`/`globalThis` seen — a hint
/// for side-effectful modules, not a guarantee (the optimizer's own
/// analysis remains authoritative).
pub const SUMMARY_SIDE_EFFECT_HINT: u32 = 1 << 3;

/// One-pass scan result. `extern` for a stable C ABI layout.
pub const ModuleSummary = extern struct {
    /// FNV-1a-based 128-bit content hash (simdHash128-compatible).
    content_hash: [16]u8,
    /// `import`/`export` keyword candidates recorded in out_offsets.
    import_candidate_count: u32,
    /// `import(` expressions (a subset of import_candidate_count).
    dynamic_import_count: u32,
    /// `require(` calls.
    require_count: u32,
    /// `export` keyword occurrences.
    export_count: u32,
    /// Bit flags (SUMMARY_*).
    flags: u32,
};

fn isIdentChar(b: u8) bool {
    return (b >= 'a' and b <= 'z') or (b >= 'A' and b <= 'Z') or
        (b >= '0' and b <= '9') or b == '_' or b == '$';
}

/// Classify what matches at `pos`. Returns via `out_offsets` bookkeeping in
/// the caller — this helper only mutates `summary`.
fn classifyAt(source: []const u8, pos: usize, summary: *ModuleSummary) bool {
    const c = source[pos];
    const boundary = pos == 0 or !isIdentChar(source[pos - 1]);

    switch (c) {
        'i' => {
            if (!boundary or pos + 6 > source.len) return false;
            if (!std.mem.eql(u8, source[pos .. pos + 6], "import")) return false;
            if (pos + 6 == source.len) return false;
            const next = source[pos + 6];
            if (isIdentChar(next)) return false; // "important", "imported"
            if (next == '.') return false; // import.meta / import.x — not a dep
            if (next == '(') {
                summary.dynamic_import_count += 1;
            }
            summary.import_candidate_count += 1;
            return true; // record offset — static or dynamic
        },
        'e' => {
            if (!boundary or pos + 6 > source.len) return false;
            // `exports.foo` → CJS signal, not an ESM export statement.
            // (The literal is 8 bytes; the old check sliced 7 bytes, so the
            // lengths never matched and the CJS flag was never set here.)
            if (pos + 8 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 8], "exports."))
            {
                summary.flags |= SUMMARY_USES_COMMONJS;
                return false;
            }
            if (!std.mem.eql(u8, source[pos .. pos + 6], "export")) return false;
            if (pos + 6 < source.len and isIdentChar(source[pos + 6])) return false;
            summary.export_count += 1;
            return true; // record offset — `export ... from` may be a dep
        },
        'r' => {
            if (!boundary) return false;
            if (pos + 8 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 8], "require("))
            {
                summary.require_count += 1;
            }
            return false;
        },
        'm' => {
            if (!boundary) return false;
            if (pos + 14 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 14], "module.exports"))
            {
                summary.flags |= SUMMARY_USES_COMMONJS;
            }
            return false;
        },
        'c' => {
            if (pos + 8 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 8], "console."))
            {
                summary.flags |= SUMMARY_SIDE_EFFECT_HINT;
            }
            return false;
        },
        'p' => {
            if (pos + 11 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 11], "process.env"))
            {
                summary.flags |= SUMMARY_SIDE_EFFECT_HINT;
            }
            return false;
        },
        'w' => {
            if (pos + 7 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 7], "window."))
            {
                summary.flags |= SUMMARY_SIDE_EFFECT_HINT;
            }
            return false;
        },
        'd' => {
            if (pos + 9 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 9], "document."))
            {
                summary.flags |= SUMMARY_SIDE_EFFECT_HINT;
            }
            return false;
        },
        'g' => {
            if (pos + 10 <= source.len and
                std.mem.eql(u8, source[pos .. pos + 10], "globalThis"))
            {
                summary.flags |= SUMMARY_SIDE_EFFECT_HINT;
            }
            return false;
        },
        '\'', '"' => {
            // 'use client' / 'use server' directives — recorded as flags
            // wherever they appear; prologue strictness is the consumer's
            // job (a string containing the phrase still flips the hint).
            const end = @min(pos + 12, source.len);
            const lit = source[pos..end];
            if (std.mem.eql(u8, lit, "'use client'") or
                std.mem.eql(u8, lit, "\"use client\""))
            {
                summary.flags |= SUMMARY_USE_CLIENT;
            } else if (std.mem.eql(u8, lit, "'use server'") or
                std.mem.eql(u8, lit, "\"use server\""))
            {
                summary.flags |= SUMMARY_USE_SERVER;
            }
            return false;
        },
        else => return false,
    }
}

/// The first-byte set the classifier responds to — used for the SIMD
/// candidate mask.
const SUMMARY_FIRST_BYTES = "iermcdwgp'\"";

fn isCandidateByte(b: u8) bool {
    inline for (SUMMARY_FIRST_BYTES) |t| {
        if (b == t) return true;
    }
    return false;
}

/// One-pass module summary. Writes import/export keyword-candidate offsets
/// to out_offsets (at most out_offsets.len), fills `summary`, and returns
/// the number of offsets written.
pub fn summarizeModule(
    source: []const u8,
    summary: *ModuleSummary,
    out_offsets: []usize,
) usize {
    summary.* = std.mem.zeroes(ModuleSummary);

    // Fused FNV-1a 128-bit hash state (4 interleaved lanes).
    var state: [4]u32 = .{ 0x811c9dc5, 0x1000193, 0x6c62272e, 0x74756c65 };
    const prime: u32 = 0x01000193;

    var count: usize = 0;
    var i: usize = 0;

    // Main sweep: hash every byte, classify at candidate positions.
    // x86_64: 32-byte AVX2 chunks with a SIMD candidate mask.
    // Other targets: scalar candidate check (still a single memory pass).
    if (builtin.cpu.arch == .x86_64) {
        while (i + 32 <= source.len) : (i += 32) {
            const chunk: @Vector(32, u8) = source[i..][0..32].*;
            // Hash (fused — same memory read as the classify pass below).
            inline for (0..32) |j| {
                const lane = j % 4;
                state[lane] = (state[lane] ^ chunk[j]) *% prime;
            }
            // Candidate positions: vector-compare the chunk against each
            // target first byte and OR-fold into a mask.
            var any: @Vector(32, u8) = @splat(0);
            inline for (SUMMARY_FIRST_BYTES) |t| {
                const cmp = chunk == @as(@Vector(32, u8), @splat(t));
                const ones: @Vector(32, u8) = @select(u8, cmp, @as(@Vector(32, u8), @splat(1)), @as(@Vector(32, u8), @splat(0)));
                any += ones;
            }
            inline for (0..32) |j| {
                if (any[j] > 0 and classifyAt(source, i + j, summary)) {
                    if (count < out_offsets.len) {
                        out_offsets[count] = i + j;
                        count += 1;
                    }
                }
            }
        }
    }

    // Scalar tail (also the entire loop on non-x86_64 targets).
    while (i < source.len) : (i += 1) {
        const b = source[i];
        state[i % 4] = (state[i % 4] ^ b) *% prime;
        if (isCandidateByte(b) and classifyAt(source, i, summary)) {
            if (count < out_offsets.len) {
                out_offsets[count] = i;
                count += 1;
            }
        }
    }

    inline for (0..4) |w| {
        std.mem.writeInt(u32, summary.content_hash[w * 4 ..][0..4], state[w], .little);
    }
    return count;
}

test "summarizeModule classifies and hashes in one pass" {
    const source =
        \\'use client';
        \\import React from 'react';
        \\import { a } from './a';
        \\const x = require('x');
        \\const m = await import('./lazy');
        \\export { a } from './a';
        \\export default function App() {}
        \\console.log('side effect');
        \\const meta = import.meta.url;
        \\const important = 1;
        \\module.exports = {};
    ;

    var summary: ModuleSummary = undefined;
    var offsets: [64]usize = undefined;
    const n = summarizeModule(source, &summary, &offsets);

    // import React, import { a }, import('./lazy'), export { a }, export default
    try std.testing.expectEqual(@as(usize, 5), n);
    try std.testing.expectEqual(@as(u32, 1), summary.dynamic_import_count);
    try std.testing.expectEqual(@as(u32, 1), summary.require_count);
    try std.testing.expectEqual(@as(u32, 2), summary.export_count);
    try std.testing.expect(summary.flags & SUMMARY_USE_CLIENT != 0);
    try std.testing.expect(summary.flags & SUMMARY_USES_COMMONJS != 0);
    try std.testing.expect(summary.flags & SUMMARY_SIDE_EFFECT_HINT != 0);
    try std.testing.expect(!std.mem.eql(
        u8,
        &summary.content_hash,
        &std.mem.zeroes([16]u8),
    ));
}

test "summarizeModule flags bare exports.x as CommonJS" {
    const source = "exports.foo = 1;";
    var summary: ModuleSummary = undefined;
    var offsets: [8]usize = undefined;
    const n = summarizeModule(source, &summary, &offsets);
    try std.testing.expectEqual(@as(usize, 0), n); // not an ESM export
    try std.testing.expect(summary.flags & SUMMARY_USES_COMMONJS != 0);
}

// ─── G1.18: SIMD-accelerated hashing ─────────────────────────────────
//
// Platform-adaptive SIMD width selection for hash computation.
// On x86_64 with AVX-512: 64-byte vectors
// On x86_64 with AVX2: 32-byte vectors
// On aarch64 with NEON: 16-byte vectors
// Falls back to scalar on unsupported platforms.
//
// blake3 is already SIMD-optimized in the Rust crate, but this provides
// a Zig-native SIMD hash for use in the native layer (graph node hashing,
// content addressing) that selects optimal vector width at comptime.

/// Compute a simple 32-bit hash of a byte slice using SIMD acceleration.
/// This is a FNV-1a variant that processes 32 bytes at a time on x86_64,
/// 16 bytes at a time on aarch64.
pub fn simdHash32(data: []const u8) u32 {
    var hash: u32 = 0x811c9dc5; // FNV offset basis

    if (builtin.cpu.arch == .x86_64 and data.len >= 32) {
        // Process 32 bytes at a time using @Vector(32, u8)
        var i: usize = 0;
        while (i + 32 <= data.len) : (i += 32) {
            const chunk: @Vector(32, u8) = data[i..][0..32].*;
            // XOR each byte into hash, then multiply — SIMD-parallel reduction
            inline for (0..32) |j| {
                hash = (hash ^ chunk[j]) *% 0x01000193; // FNV prime
            }
        }
        // Remaining bytes
        while (i < data.len) : (i += 1) {
            hash = (hash ^ data[i]) *% 0x01000193;
        }
    } else if (builtin.cpu.arch == .aarch64 and data.len >= 16) {
        // Process 16 bytes at a time using @Vector(16, u8) (NEON)
        var i: usize = 0;
        while (i + 16 <= data.len) : (i += 16) {
            const chunk: @Vector(16, u8) = data[i..][0..16].*;
            inline for (0..16) |j| {
                hash = (hash ^ chunk[j]) *% 0x01000193;
            }
        }
        while (i < data.len) : (i += 1) {
            hash = (hash ^ data[i]) *% 0x01000193;
        }
    } else {
        // Scalar fallback
        for (data) |b| {
            hash = (hash ^ b) *% 0x01000193;
        }
    }

    return hash;
}

/// Compute a 128-bit hash (4 × u32) using SIMD acceleration.
/// Returns a [16]u8 array suitable for use as a TaskId.
pub fn simdHash128(data: []const u8) [16]u8 {
    var state: [4]u32 = .{ 0x811c9dc5, 0x1000193, 0x6c62272e, 0x74756c65 };
    const primes: [4]u32 = .{ 0x01000193, 0x01000193, 0x01000193, 0x01000193 };

    if (builtin.cpu.arch == .x86_64 and data.len >= 32) {
        var i: usize = 0;
        while (i + 32 <= data.len) : (i += 32) {
            const chunk: @Vector(32, u8) = data[i..][0..32].*;
            inline for (0..32) |j| {
                const lane = j % 4;
                state[lane] = (state[lane] ^ chunk[j]) *% primes[lane];
            }
        }
        while (i < data.len) : (i += 1) {
            state[i % 4] = (state[i % 4] ^ data[i]) *% primes[i % 4];
        }
    } else {
        for (data, 0..) |b, idx| {
            state[idx % 4] = (state[idx % 4] ^ b) *% primes[idx % 4];
        }
    }

    var result: [16]u8 = undefined;
    inline for (0..4) |w| {
        std.mem.writeInt(u32, result[w * 4 ..][0..4], state[w], .little);
    }
    return result;
}

/// Detect the optimal SIMD width for the current platform at comptime.
pub fn optimalSimdWidth() usize {
    if (builtin.cpu.arch == .x86_64) {
        // AVX-512 would be 64, but we use AVX2 (32) for broad compatibility
        return 32;
    } else if (builtin.cpu.arch == .aarch64) {
        return 16; // NEON
    }
    return 8; // Scalar fallback width
}

// ─── G8.11: SIMD status scan for arena graph ─────────────────────────
//
// When finding all dirty nodes in the task graph, scan the packed
// status fields using @Vector for parallel comparison.
// The status is stored in the upper 3 bits of each node's packed_flags u32.
// STATUS_SHIFT = 29 (15 + 14), STATUS_MASK = 0x7

const STATUS_SHIFT: u5 = 29;

/// Find all node indices with a specific status value using SIMD.
/// Scans packed_flags array in 8-wide chunks on x86_64, 4-wide on aarch64.
/// Returns the count of matching indices written to out_indices.
pub fn findNodesByStatus(
    packed_flags: []const u32,
    target_status: u3,
    out_indices: []u32,
) usize {
    var count: usize = 0;
    const simd_width: usize = if (builtin.cpu.arch == .x86_64) 8 else 4;

    var i: usize = 0;
    // SIMD-parallel scan: process 8 (or 4) u32 values at once
    while (i + simd_width <= packed_flags.len) : (i += simd_width) {
        if (builtin.cpu.arch == .x86_64 and simd_width == 8) {
            const chunk: @Vector(8, u32) = packed_flags[i..][0..8].*;
            // Extract status bits by shifting right, then compare
            const shifted: @Vector(8, u32) = chunk >> @as(@Vector(8, u5), @splat(STATUS_SHIFT));
            const target_vec: @Vector(8, u32) = @splat(@as(u32, target_status));
            const cmp = shifted == target_vec;
            // Extract matching indices
            inline for (0..8) |j| {
                if (cmp[j] and count < out_indices.len) {
                    out_indices[count] = @intCast(i + j);
                    count += 1;
                }
            }
        } else if (builtin.cpu.arch == .aarch64 and simd_width == 4) {
            const chunk: @Vector(4, u32) = packed_flags[i..][0..4].*;
            const shifted: @Vector(4, u32) = chunk >> @as(@Vector(4, u5), @splat(STATUS_SHIFT));
            const target_vec: @Vector(4, u32) = @splat(@as(u32, target_status));
            const cmp = shifted == target_vec;
            inline for (0..4) |j| {
                if (cmp[j] and count < out_indices.len) {
                    out_indices[count] = @intCast(i + j);
                    count += 1;
                }
            }
        } else {
            // Scalar fallback within the loop
            var j: usize = 0;
            while (j < simd_width) : (j += 1) {
                const status: u3 = @intCast((packed_flags[i + j] >> STATUS_SHIFT) & 0x7);
                if (status == target_status and count < out_indices.len) {
                    out_indices[count] = @intCast(i + j);
                    count += 1;
                }
            }
        }
    }

    // Scalar tail
    while (i < packed_flags.len) : (i += 1) {
        const status: u3 = @intCast((packed_flags[i] >> STATUS_SHIFT) & 0x7);
        if (status == target_status and count < out_indices.len) {
            out_indices[count] = @intCast(i);
            count += 1;
        }
    }

    return count;
}

/// Find all dirty nodes (status == 1) using SIMD scan.
pub fn findDirtyNodes(packed_flags: []const u32, out_indices: []u32) usize {
    return findNodesByStatus(packed_flags, 1, out_indices);
}

test "simdHash32 produces consistent hash" {
    const data = "hello pledgepack";
    const h1 = simdHash32(data);
    const h2 = simdHash32(data);
    try std.testing.expectEqual(h1, h2);
    // Different data should produce different hash
    const h3 = simdHash32("hello pledgepac!");
    try std.testing.expect(h1 != h3);
}

test "simdHash32 handles empty input" {
    const h = simdHash32("");
    try std.testing.expectEqual(@as(u32, 0x811c9dc5), h);
}

test "simdHash128 produces 16-byte hash" {
    const data = "test data for 128-bit hash";
    const h1 = simdHash128(data);
    const h2 = simdHash128(data);
    try std.testing.expectEqualSlices(u8, &h1, &h2);
    // Different data should produce different hash
    const h3 = simdHash128("different data here!!!");
    try std.testing.expect(!std.mem.eql(u8, &h1, &h3));
}

test "optimalSimdWidth returns platform-appropriate value" {
    const w = optimalSimdWidth();
    if (builtin.cpu.arch == .x86_64) {
        try std.testing.expectEqual(@as(usize, 32), w);
    } else if (builtin.cpu.arch == .aarch64) {
        try std.testing.expectEqual(@as(usize, 16), w);
    } else {
        try std.testing.expectEqual(@as(usize, 8), w);
    }
}

test "findNodesByStatus finds dirty nodes" {
    // Simulate packed_flags: 3 clean, 2 dirty, 1 computing, 2 dirty
    var flags: [8]u32 = undefined;
    flags[0] = 0; // clean (status=0)
    flags[1] = 0;
    flags[2] = 0;
    flags[3] = @as(u32, 1) << STATUS_SHIFT; // dirty (status=1)
    flags[4] = @as(u32, 1) << STATUS_SHIFT; // dirty
    flags[5] = @as(u32, 2) << STATUS_SHIFT; // computing (status=2)
    flags[6] = @as(u32, 1) << STATUS_SHIFT; // dirty
    flags[7] = @as(u32, 1) << STATUS_SHIFT; // dirty

    var indices: [8]u32 = undefined;
    const count = findDirtyNodes(&flags, &indices);
    try std.testing.expectEqual(@as(usize, 4), count);
    try std.testing.expectEqual(@as(u32, 3), indices[0]);
    try std.testing.expectEqual(@as(u32, 4), indices[1]);
    try std.testing.expectEqual(@as(u32, 6), indices[2]);
    try std.testing.expectEqual(@as(u32, 7), indices[3]);
}

test "findNodesByStatus finds pending nodes" {
    var flags: [4]u32 = undefined;
    flags[0] = @as(u32, 4) << STATUS_SHIFT; // pending
    flags[1] = 0; // clean
    flags[2] = @as(u32, 4) << STATUS_SHIFT; // pending
    flags[3] = 0; // clean

    var indices: [4]u32 = undefined;
    const count = findNodesByStatus(&flags, 4, &indices);
    try std.testing.expectEqual(@as(usize, 2), count);
    try std.testing.expectEqual(@as(u32, 0), indices[0]);
    try std.testing.expectEqual(@as(u32, 2), indices[1]);
}
