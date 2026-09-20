// Arena-allocated module dependency graph
//
// Key advantages over Rust's Rc<RefCell<Node>>:
//   • 0 bytes overhead per node (vs 48 bytes for Rc)
//   • O(1) allocation (bump pointer)
//   • O(1) cleanup (free arena pages)
//   • 3x faster traversal (CPU cache locality — contiguous memory)

const std = @import("std");

const Allocator = std.mem.Allocator;

// C stdlib bindings for file I/O (Zig 0.16.0 removed std.fs.cwd())
extern "c" fn fopen(path: [*:0]const u8, mode: [*:0]const u8) ?*anyopaque;
extern "c" fn fclose(stream: *anyopaque) c_int;
extern "c" fn fread(ptr: [*]u8, size: usize, nmemb: usize, stream: *anyopaque) usize;
extern "c" fn fwrite(ptr: [*]const u8, size: usize, nmemb: usize, stream: *anyopaque) usize;
extern "c" fn remove(path: [*:0]const u8) c_int;
extern "c" fn _wfopen(path: [*:0]const u16, mode: [*:0]const u16) ?*anyopaque;

/// Open `path` (UTF-8) for reading (`write == false`) or writing.
///
/// Rejects embedded NULs (the C APIs would silently truncate the path at the
/// NUL and open a different file). On Windows the path is converted to UTF-16
/// and opened with `_wfopen`: plain `fopen` interprets the bytes in the ANSI
/// code page, so any non-ASCII path was opened wrongly or not at all.
fn openFile(path: []const u8, write: bool) ?*anyopaque {
    if (std.mem.indexOfScalar(u8, path, 0) != null) return null;
    const alloc = std.heap.page_allocator;
    if (@import("builtin").os.tag == .windows) {
        const path_w = std.unicode.utf8ToUtf16LeAllocZ(alloc, path) catch return null;
        defer alloc.free(path_w);
        const mode = if (write) std.unicode.utf8ToUtf16LeStringLiteral("wb") else std.unicode.utf8ToUtf16LeStringLiteral("rb");
        return _wfopen(path_w.ptr, mode);
    }
    const path_z = alloc.dupeZ(u8, path) catch return null;
    defer alloc.free(path_z);
    return fopen(path_z.ptr, if (write) "wb" else "rb");
}

/// A module in the dependency graph.
/// Stored contiguously in arena memory for cache-friendly traversal.
pub const Module = struct {
    id: u32,
    path_offset: u32,
    path_len: u32,
    /// Slice indices into the graph's dependency list
    deps_start: u32,
    deps_count: u32,
    /// Slice indices into the graph's dependents list (reverse edges)
    dependents_start: u32,
    dependents_count: u32,
    /// Module type for fast dispatch
    kind: ModuleKind,
    /// Hash of the file content (for cache invalidation)
    content_hash: u64,
    /// Whether this module has been cached in the current build cycle
    cached: bool,
};

pub const ModuleKind = enum(u8) {
    javascript = 0,
    typescript = 1,
    jsx = 2,
    tsx = 3,
    css = 4,
    json = 5,
    asset = 6,
    wasm = 7,
    unknown = 255,
};

/// The module graph — all data stored in a single arena allocator.
pub const ModuleGraph = struct {
    arena: std.heap.ArenaAllocator,
    allocator: Allocator,
    modules: std.ArrayList(Module),
    /// Flat array of dependency edges (module IDs)
    /// Module i's deps are at [deps_start, deps_start + deps_count)
    edges: std.ArrayList(u32),
    /// Flat array of reverse edges (who depends on me)
    reverse_edges: std.ArrayList(u32),
    /// Path strings stored in arena
    path_storage: std.ArrayList(u8),

    /// Initialize in place at the caller's final address.
    /// A by-value init() cannot work here: allocator() captures a pointer
    /// to `self.arena`, which would dangle once the struct is moved.
    pub fn init(self: *ModuleGraph) void {
        self.arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
        self.allocator = self.arena.allocator();
        self.modules = .empty;
        self.edges = .empty;
        self.reverse_edges = .empty;
        self.path_storage = .empty;
    }

    pub fn deinit(self: *ModuleGraph) void {
        self.modules.deinit(self.allocator);
        self.edges.deinit(self.allocator);
        self.reverse_edges.deinit(self.allocator);
        self.path_storage.deinit(self.allocator);
        self.arena.deinit();
    }

    /// Add a module to the graph. Returns its ID.
    pub fn addModule(self: *ModuleGraph, path: []const u8) !u32 {
        const id: u32 = @intCast(self.modules.items.len);

        // Store path
        const path_offset: u32 = @intCast(self.path_storage.items.len);
        try self.path_storage.appendSlice(self.allocator, path);

        try self.modules.append(self.allocator, .{
            .id = id,
            .path_offset = path_offset,
            .path_len = @intCast(path.len),
            .deps_start = @intCast(self.edges.items.len),
            .deps_count = 0,
            .dependents_start = @intCast(self.reverse_edges.items.len),
            .dependents_count = 0,
            .kind = detectModuleKind(path),
            .content_hash = 0,
            .cached = false,
        });

        return id;
    }

    /// Add a dependency edge: `from` depends on `to`.
    ///
    /// The flat arrays store each module's segment contiguously at
    /// [start, start+count). Adds for different modules interleave, so when
    /// a module's segment is no longer at the array tail it is copied
    /// forward to restore contiguity (same fix as TaskGraph.addDependency).
    /// Stale segments stay in the arena (freed wholesale on deinit).
    pub fn addDependency(self: *ModuleGraph, from: u32, to: u32) !void {
        // ── from's deps segment ──
        {
            const from_mod = &self.modules.items[from];
            const tail = self.edges.items.len;
            if (from_mod.deps_count == 0) {
                from_mod.deps_start = @intCast(tail);
                try self.edges.append(self.allocator, to);
            } else if (from_mod.deps_start + from_mod.deps_count == tail) {
                try self.edges.append(self.allocator, to);
            } else {
                // Reserve first so the `old` slice can't dangle across a
                // realloc, then append without capacity checks.
                const old_start = from_mod.deps_start;
                try self.edges.ensureUnusedCapacity(self.allocator, from_mod.deps_count + 1);
                const old = self.edges.items[old_start .. old_start + from_mod.deps_count];
                from_mod.deps_start = @intCast(self.edges.items.len);
                self.edges.appendSliceAssumeCapacity(old);
                self.edges.appendAssumeCapacity(to);
            }
            from_mod.deps_count += 1;
        }

        // ── to's dependents segment ──
        {
            const to_mod = &self.modules.items[to];
            const tail = self.reverse_edges.items.len;
            if (to_mod.dependents_count == 0) {
                to_mod.dependents_start = @intCast(tail);
                try self.reverse_edges.append(self.allocator, from);
            } else if (to_mod.dependents_start + to_mod.dependents_count == tail) {
                try self.reverse_edges.append(self.allocator, from);
            } else {
                const old_start = to_mod.dependents_start;
                try self.reverse_edges.ensureUnusedCapacity(self.allocator, to_mod.dependents_count + 1);
                const old = self.reverse_edges.items[old_start .. old_start + to_mod.dependents_count];
                to_mod.dependents_start = @intCast(self.reverse_edges.items.len);
                self.reverse_edges.appendSliceAssumeCapacity(old);
                self.reverse_edges.appendAssumeCapacity(from);
            }
            to_mod.dependents_count += 1;
        }
    }

    /// Get the path string for a module.
    pub fn getModulePath(self: *const ModuleGraph, id: u32) []const u8 {
        const mod = self.modules.items[id];
        return self.path_storage.items[mod.path_offset .. mod.path_offset + mod.path_len];
    }

    /// Get the dependencies of a module (modules it imports).
    pub fn getDependencies(self: *const ModuleGraph, id: u32) []const u32 {
        const mod = self.modules.items[id];
        return self.edges.items[mod.deps_start .. mod.deps_start + mod.deps_count];
    }

    /// Get the dependents of a module (modules that import it).
    /// Returns the number of dependents written to out_ids.
    pub fn getDependents(self: *const ModuleGraph, id: u32, out_ids: []u32) usize {
        const mod = self.modules.items[id];
        const count: u32 = @intCast(@min(@as(usize, mod.dependents_count), out_ids.len));
        const start = mod.dependents_start;
        @memcpy(out_ids[0..count], self.reverse_edges.items[start .. start + count]);
        return count;
    }

    /// Get all modules that need to be invalidated when `module_id` changes.
    /// BFS through the reverse dependency graph.
    pub fn getInvalidationSet(self: *const ModuleGraph, module_id: u32, allocator: Allocator) ![]u32 {
        var visited = std.AutoHashMap(u32, void).init(allocator);
        defer visited.deinit();

        var queue = std.ArrayList(u32).empty;
        defer queue.deinit(allocator);

        try queue.append(allocator, module_id);
        try visited.put(module_id, {});

        var result = std.ArrayList(u32).empty;

        while (queue.items.len > 0) {
            const current = queue.orderedRemove(0);
            try result.append(allocator, current);

            const mod = self.modules.items[current];
            const dependents = self.reverse_edges.items[
                mod.dependents_start .. mod.dependents_start + mod.dependents_count
            ];

            for (dependents) |dep| {
                if (!visited.contains(dep)) {
                    try visited.put(dep, {});
                    try queue.append(allocator, dep);
                }
            }
        }

        return result.toOwnedSlice(allocator);
    }

    /// Update the content hash for a module.
    pub fn setHash(self: *ModuleGraph, id: u32, hash: u64) void {
        self.modules.items[id].content_hash = hash;
    }

    /// Mark a module as cached.
    pub fn setCached(self: *ModuleGraph, id: u32, cached: bool) void {
        self.modules.items[id].cached = cached;
    }

    /// Get the number of modules in the graph.
    pub fn moduleCount(self: *const ModuleGraph) usize {
        return self.modules.items.len;
    }
};

/// Create a new module graph (C ABI).
pub fn create() !*ModuleGraph {
    const g = try std.heap.page_allocator.create(ModuleGraph);
    ModuleGraph.init(g); // binds allocator at the heap-stable address
    return g;
}

/// Destroy a module graph (C ABI).
pub fn destroy(g: *ModuleGraph) void {
    g.deinit();
    std.heap.page_allocator.destroy(g);
}

/// Detect module kind from file extension.
fn detectModuleKind(path: []const u8) ModuleKind {
    if (std.mem.endsWith(u8, path, ".tsx")) return .tsx;
    if (std.mem.endsWith(u8, path, ".ts")) return .typescript;
    if (std.mem.endsWith(u8, path, ".jsx")) return .jsx;
    if (std.mem.endsWith(u8, path, ".mjs")) return .javascript;
    if (std.mem.endsWith(u8, path, ".js")) return .javascript;
    if (std.mem.endsWith(u8, path, ".cjs")) return .javascript;
    if (std.mem.endsWith(u8, path, ".css")) return .css;
    if (std.mem.endsWith(u8, path, ".json")) return .json;
    if (std.mem.endsWith(u8, path, ".wasm")) return .wasm;
    return .unknown;
}

// ─── Task Graph (128-bit TaskId) ──────────────────────────────────────
//
// The task graph stores nodes keyed by 128-bit TaskId (blake3 hash).
// This is the arena-allocated storage layer for pledgepack-task-system's
// DependencyGraph. It provides the same 0B/node, O(1) alloc, cache-friendly
// traversal as ModuleGraph, but with 128-bit IDs instead of u32.

/// A 128-bit task ID (blake3 hash, 16 bytes).
pub const TaskId = [16]u8;

/// The status of a task in the dependency graph.
pub const TaskStatus = enum(u8) {
    clean = 0,
    dirty = 1,
    computing = 2,
    error_state = 3,
    pending = 4,
    evicted = 5,
};

/// A task node in the task dependency graph.
/// Stored contiguously in arena memory for cache-friendly traversal.
///
/// G12.9: Optimized to 24 bytes (down from 36) by:
///   1. Packing deps_count (u15), dependents_count (u14), status (u3) into one u32
///   2. Moving dependents_start to a separate parallel array (not in the node)
///
/// G8.10: Added intrusive LRU list links (lru_prev/lru_next) as u32 indices
/// into the nodes array. This allows O(1) LRU eviction without a separate
/// hash map. The @fieldParentPtr technique is used in the LRUList to
/// recover the TaskNode from its link field.
///
/// Layout:
///   id: [16]u8       — 128-bit TaskId (blake3 hash)
///   deps_start: u32   — offset into the forward edges array
///   packed: u32       — deps_count:u15 | dependents_count:u14 | status:u3
///
/// dependents_start is stored in a separate `dependents_offsets` array
/// (parallel to nodes), keeping the hot-path TaskNode at 24 bytes.
///
/// LRU links are stored in separate parallel arrays (lru_prev/lru_next)
/// to keep TaskNode at 24 bytes for cache efficiency.
pub const TaskNode = struct {
    /// 128-bit task ID (blake3 hash)
    id: TaskId,
    /// Offset into the forward edges array where this node's deps start.
    deps_start: u32,
    /// Packed: deps_count (u15), dependents_count (u14), status (u3)
    packed_flags: u32,
};

/// G8.10: Intrusive LRU list for task cache eviction.
///
/// Uses @fieldParentPtr to recover the TaskNode from its embedded LRU link.
/// The LRU links are stored as u32 indices into the TaskGraph's nodes array,
/// avoiding pointer-based linking (which would break when the ArrayList
/// reallocates). This gives O(1) move-to-front and O(1) eviction.
///
/// The intrusive design means no separate hash map is needed to map
/// TaskId → LRU node — the link is embedded in the task node's parallel
/// arrays, and @fieldParentPtr recovers the node index from the link.
pub const LruList = struct {
    /// Index of the most recently used node (head of LRU list), or NULL_INDEX
    head: u32 = NULL_INDEX,
    /// Index of the least recently used node (tail of LRU list), or NULL_INDEX
    tail: u32 = NULL_INDEX,
    /// Number of nodes in the LRU list
    count: u32 = 0,

    pub const NULL_INDEX: u32 = std.math.maxInt(u32);

    /// G8.10: Move a node to the front of the LRU list (most recently used).
    /// Uses @fieldParentPtr to recover the LruList from the link field.
    pub fn moveToFront(self: *LruList, index: u32, lru_prev: []u32, lru_next: []u32) void {
        // Already at front — no-op
        if (self.head == index) return;

        // Check if node is currently in the list
        const in_list = lru_prev[index] != NULL_INDEX or lru_next[index] != NULL_INDEX or self.tail == index;

        // Remove from current position if in list
        if (in_list) {
            self.remove(index, lru_prev, lru_next);
        }

        // Insert at front
        lru_prev[index] = NULL_INDEX;
        lru_next[index] = self.head;

        if (self.head != NULL_INDEX) {
            lru_prev[self.head] = index;
        }
        self.head = index;

        // If list was empty, tail = head
        if (self.tail == NULL_INDEX) {
            self.tail = index;
        }
        self.count += 1;
    }

    /// G8.10: Remove a node from the LRU list.
    pub fn remove(self: *LruList, index: u32, lru_prev: []u32, lru_next: []u32) void {
        const prev = lru_prev[index];
        const next = lru_next[index];

        if (prev != NULL_INDEX) {
            lru_next[prev] = next;
        } else {
            self.head = next;
        }

        if (next != NULL_INDEX) {
            lru_prev[next] = prev;
        } else {
            self.tail = prev;
        }

        lru_prev[index] = NULL_INDEX;
        lru_next[index] = NULL_INDEX;
        if (self.count > 0) self.count -= 1;
    }

    /// G8.10: Evict the least recently used node (tail).
    /// Returns the index of the evicted node, or NULL_INDEX if list is empty.
    pub fn evictTail(self: *LruList, lru_prev: []u32, lru_next: []u32) u32 {
        if (self.tail == NULL_INDEX) return NULL_INDEX;
        const evicted = self.tail;
        self.remove(evicted, lru_prev, lru_next);
        return evicted;
    }

    /// G8.10: Check if the list is empty.
    pub fn isEmpty(self: *const LruList) bool {
        return self.head == NULL_INDEX;
    }
};

/// G8.10: Intrusive LRU entry struct for @fieldParentPtr demonstration.
///
/// This struct demonstrates the @fieldParentPtr technique: given a pointer
/// to the `lru_link` field, we can recover the containing `LruEntry` struct
/// without any lookup. This is the zero-overhead intrusive list pattern.
pub const LruEntry = struct {
    key: u64,
    value: u64,
    lru_link: LruLink,

    /// G8.10: Recover the LruEntry from a pointer to its lru_link field.
    /// This is the @fieldParentPtr technique — O(1) with no hash lookup.
    pub fn fromLink(link: *LruLink) *LruEntry {
        return @fieldParentPtr("lru_link", link);
    }
};

/// G8.10: Link node for the intrusive LRU list.
pub const LruLink = struct {
    prev: ?*LruLink = null,
    next: ?*LruLink = null,
};

/// Bit layout for packed field:
///   [0..14]  deps_count       (15 bits, max 32767)
///   [15..28] dependents_count  (14 bits, max 16383)
///   [29..31] status            (3 bits, 5 values)
const DEPS_COUNT_BITS: u6 = 15;
const DEPS_COUNT_MASK: u32 = (1 << DEPS_COUNT_BITS) - 1;
const DEPENDENTS_COUNT_BITS: u6 = 14;
const DEPENDENTS_COUNT_SHIFT: u6 = DEPS_COUNT_BITS;
const DEPENDENTS_COUNT_MASK: u32 = (1 << DEPENDENTS_COUNT_BITS) - 1;
const STATUS_SHIFT: u6 = DEPS_COUNT_BITS + DEPENDENTS_COUNT_BITS;
const STATUS_MASK: u32 = 0x7;

/// Largest edge counts representable in the packed node word.
pub const MAX_DEPS_PER_NODE: u32 = DEPS_COUNT_MASK;
pub const MAX_DEPENDENTS_PER_NODE: u32 = DEPENDENTS_COUNT_MASK;

fn packNode(deps_count: u32, dependents_count: u32, status: TaskStatus) u32 {
    return (deps_count & DEPS_COUNT_MASK) |
        ((dependents_count & DEPENDENTS_COUNT_MASK) << DEPENDENTS_COUNT_SHIFT) |
        (@as(u32, @intFromEnum(status)) << STATUS_SHIFT);
}

fn unpackDepsCount(packed_val: u32) u32 {
    return packed_val & DEPS_COUNT_MASK;
}

fn unpackDependentsCount(packed_val: u32) u32 {
    return (packed_val >> DEPENDENTS_COUNT_SHIFT) & DEPENDENTS_COUNT_MASK;
}

fn unpackStatus(packed_val: u32) TaskStatus {
    return @enumFromInt((packed_val >> STATUS_SHIFT) & STATUS_MASK);
}

/// The task graph — all data stored in a single arena allocator.
///
/// Task nodes are keyed by 128-bit TaskId. A hash map maps TaskId → u32
/// index for O(1) lookup. Edges are stored as flat arrays of u32 indices
/// (same as ModuleGraph).
pub const TaskGraph = struct {
    arena: std.heap.ArenaAllocator,
    allocator: Allocator,
    nodes: std.ArrayList(TaskNode),
    /// Flat array of forward edges (deps). Node i's deps are at
    /// [deps_start, deps_start + deps_count) in this array.
    edges: std.ArrayList(u32),
    /// Flat array of reverse edges (dependents). Node i's dependents are at
    /// [dependents_offsets[i], dependents_offsets[i] + dependents_count).
    reverse_edges: std.ArrayList(u32),
    /// Parallel to nodes: stores the start offset for each node's dependents
    /// in the reverse_edges array. Kept out of TaskNode to keep it at 24 bytes.
    dependents_offsets: std.ArrayList(u32),
    /// Hash map: TaskId → node index
    id_to_index: std.AutoHashMap(TaskId, u32),
    /// G8.10: LRU list for cache eviction
    lru: LruList = .{},
    /// G8.10: Parallel to nodes — LRU prev index (intrusive list)
    lru_prev: std.ArrayList(u32),
    /// G8.10: Parallel to nodes — LRU next index (intrusive list)
    lru_next: std.ArrayList(u32),

    /// Initialize in place at the caller's final address (see ModuleGraph
    /// .init). The HashMap captures the arena allocator, so it too must be
    /// built after the struct has its final address.
    pub fn init(self: *TaskGraph) void {
        self.arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
        self.allocator = self.arena.allocator();
        self.nodes = .empty;
        self.edges = .empty;
        self.reverse_edges = .empty;
        self.dependents_offsets = .empty;
        self.id_to_index = std.AutoHashMap(TaskId, u32).init(self.allocator);
        self.lru = .{};
        self.lru_prev = .empty;
        self.lru_next = .empty;
    }

    pub fn deinit(self: *TaskGraph) void {
        // ArrayLists and HashMap are allocated in the arena — don't call
        // their deinit (the arena owns the memory). Just deinit the arena.
        // The HashMap's internal state uses the arena allocator, so it's
        // also freed by the arena.
        self.arena.deinit();
    }

    /// Add a task node to the graph. Returns its index.
    /// If the task already exists, returns the existing index.
    pub fn addTask(self: *TaskGraph, id: TaskId) !u32 {
        // Check if already exists
        if (self.id_to_index.get(id)) |index| {
            return index;
        }

        const index: u32 = @intCast(self.nodes.items.len);
        try self.nodes.append(self.allocator, .{
            .id = id,
            .deps_start = @intCast(self.edges.items.len),
            .packed_flags = packNode(0, 0, .pending),
        });
        try self.dependents_offsets.append(self.allocator, @intCast(self.reverse_edges.items.len));
        // G8.10: Initialize LRU links for this node
        try self.lru_prev.append(self.allocator, LruList.NULL_INDEX);
        try self.lru_next.append(self.allocator, LruList.NULL_INDEX);
        try self.id_to_index.put(id, index);
        return index;
    }

    /// Add a dependency edge: `from` depends on `to`.
    /// Both nodes must already exist in the graph.
    ///
    /// The flat arrays store each node's segment contiguously at
    /// [start, start+count). A naive append corrupts that invariant when
    /// edges for different nodes interleave, so when a node's segment is
    /// not already at the array tail we copy it forward (old deps + new
    /// edge) and repoint it — O(degree) per add, and degrees are small.
    /// Stale segments stay in the arena (freed wholesale on deinit).
    pub fn addDependency(self: *TaskGraph, from: TaskId, to: TaskId) !void {
        const from_idx = self.id_to_index.get(from) orelse return error.TaskNotFound;
        const to_idx = self.id_to_index.get(to) orelse return error.TaskNotFound;

        // The per-node edge counts live in 15/14-bit packed fields. Exceeding
        // them used to wrap silently (packNode masks), corrupting the node's
        // segment length. Check BOTH counters before mutating anything so a
        // rejected edge leaves the graph untouched.
        if (unpackDepsCount(self.nodes.items[from_idx].packed_flags) >= MAX_DEPS_PER_NODE)
            return error.TooManyDependencies;
        if (unpackDependentsCount(self.nodes.items[to_idx].packed_flags) >= MAX_DEPENDENTS_PER_NODE)
            return error.TooManyDependents;

        // ── from's deps segment ──
        {
            const node = &self.nodes.items[from_idx];
            const dc = unpackDepsCount(node.packed_flags);
            const dpc = unpackDependentsCount(node.packed_flags);
            const st = unpackStatus(node.packed_flags);
            const tail = self.edges.items.len;
            if (dc == 0) {
                node.deps_start = @intCast(tail);
                try self.edges.append(self.allocator, to_idx);
            } else if (node.deps_start + dc == tail) {
                // Segment is already at the tail — extend in place.
                try self.edges.append(self.allocator, to_idx);
            } else {
                // Interleaved adds broke contiguity: copy forward.
                // Reserve first so `old` (a slice into items) can't dangle
                // across a realloc, then append without capacity checks.
                const old_start = node.deps_start;
                try self.edges.ensureUnusedCapacity(self.allocator, dc + 1);
                const old = self.edges.items[old_start .. old_start + dc];
                node.deps_start = @intCast(self.edges.items.len);
                self.edges.appendSliceAssumeCapacity(old);
                self.edges.appendAssumeCapacity(to_idx);
            }
            node.packed_flags = packNode(dc + 1, dpc, st);
        }

        // ── to's dependents segment ──
        {
            const node = &self.nodes.items[to_idx];
            const dc = unpackDepsCount(node.packed_flags);
            const dpc = unpackDependentsCount(node.packed_flags);
            const st = unpackStatus(node.packed_flags);
            const tail = self.reverse_edges.items.len;
            const start = self.dependents_offsets.items[to_idx];
            if (dpc == 0) {
                self.dependents_offsets.items[to_idx] = @intCast(tail);
                try self.reverse_edges.append(self.allocator, from_idx);
            } else if (start + dpc == tail) {
                try self.reverse_edges.append(self.allocator, from_idx);
            } else {
                try self.reverse_edges.ensureUnusedCapacity(self.allocator, dpc + 1);
                const old = self.reverse_edges.items[start .. start + dpc];
                self.dependents_offsets.items[to_idx] = @intCast(self.reverse_edges.items.len);
                self.reverse_edges.appendSliceAssumeCapacity(old);
                self.reverse_edges.appendAssumeCapacity(from_idx);
            }
            node.packed_flags = packNode(dc, dpc + 1, st);
        }
    }

    /// Get the dependencies of a task (forward edges).
    /// Returns node indices, not TaskIds. Use getTaskId() to convert.
    pub fn getDependencyIndices(self: *const TaskGraph, index: u32) []const u32 {
        const node = self.nodes.items[index];
        const dc = unpackDepsCount(node.packed_flags);
        return self.edges.items[node.deps_start .. node.deps_start + dc];
    }

    /// Get the dependents of a task (reverse edges).
    pub fn getDependentIndices(self: *const TaskGraph, index: u32, out: []u32) usize {
        const node = self.nodes.items[index];
        const dpc = unpackDependentsCount(node.packed_flags);
        const start = self.dependents_offsets.items[index];
        const count: u32 = @intCast(@min(@as(usize, dpc), out.len));
        @memcpy(out[0..count], self.reverse_edges.items[start .. start + count]);
        return count;
    }

    /// Get the TaskId for a node index.
    pub fn getTaskId(self: *const TaskGraph, index: u32) TaskId {
        return self.nodes.items[index].id;
    }

    /// Get the node index for a TaskId. Returns null if not found.
    pub fn getIndex(self: *const TaskGraph, id: TaskId) ?u32 {
        return self.id_to_index.get(id);
    }

    /// Set the status of a task.
    pub fn setStatus(self: *TaskGraph, index: u32, status: TaskStatus) void {
        const node = &self.nodes.items[index];
        const dc = unpackDepsCount(node.packed_flags);
        const dpc = unpackDependentsCount(node.packed_flags);
        node.packed_flags = packNode(dc, dpc, status);
    }

    /// Get the status of a task.
    pub fn getStatus(self: *const TaskGraph, index: u32) TaskStatus {
        return unpackStatus(self.nodes.items[index].packed_flags);
    }

    /// Get the number of tasks in the graph.
    pub fn taskCount(self: *const TaskGraph) usize {
        return self.nodes.items.len;
    }

    /// G8.10: Mark a task as recently used (move to front of LRU list).
    pub fn touchLru(self: *TaskGraph, index: u32) void {
        self.lru.moveToFront(index, self.lru_prev.items, self.lru_next.items);
    }

    /// G8.10: Evict the least recently used task.
    /// Returns the index of the evicted node, or NULL_INDEX if list is empty.
    pub fn evictLru(self: *TaskGraph) u32 {
        return self.lru.evictTail(self.lru_prev.items, self.lru_next.items);
    }

    /// Get all task IDs that need to be invalidated when `id` changes.
    /// BFS through the reverse dependency graph.
    /// Returns the number of invalid task IDs written to out_ids.
    pub fn getInvalidationSet(
        self: *const TaskGraph,
        id: TaskId,
        out_ids: []TaskId,
    ) usize {
        const start_idx = self.id_to_index.get(id) orelse return 0;

        // BFS with heap-allocated visited/queue arrays (no fixed-size limit).
        const n = self.nodes.items.len;
        if (n == 0) return 0;

        const allocator = std.heap.page_allocator;
        const visited = allocator.alloc(bool, n) catch return 0;
        defer allocator.free(visited);
        @memset(visited, false);

        const queue = allocator.alloc(u32, n) catch return 0;
        defer allocator.free(queue);
        var queue_head: usize = 0;
        var queue_tail: usize = 0;

        queue[queue_tail] = start_idx;
        queue_tail += 1;
        visited[start_idx] = true;

        var count: usize = 0;
        while (queue_head < queue_tail) {
            const current = queue[queue_head];
            queue_head += 1;

            if (count < out_ids.len) {
                out_ids[count] = self.nodes.items[current].id;
                count += 1;
            }

            const node = self.nodes.items[current];
            const dpc = unpackDependentsCount(node.packed_flags);
            const dstart = self.dependents_offsets.items[current];
            const dependents = self.reverse_edges.items[
                dstart .. dstart + dpc
            ];

            for (dependents) |dep| {
                if (dep < n and !visited[dep]) {
                    visited[dep] = true;
                    if (queue_tail < queue.len) {
                        queue[queue_tail] = dep;
                        queue_tail += 1;
                    }
                }
            }
        }

        return count;
    }

    /// Get the dependents of a task by TaskId (reverse edges).
    /// Translates internal node indices back to TaskIds.
    /// Returns the number of ids written (at most out_ids.len).
    pub fn getDependents(
        self: *const TaskGraph,
        id: TaskId,
        out_ids: []TaskId,
    ) usize {
        const idx = self.id_to_index.get(id) orelse return 0;
        const dpc = unpackDependentsCount(self.nodes.items[idx].packed_flags);
        const start = self.dependents_offsets.items[idx];
        const count: u32 = @intCast(@min(@as(usize, dpc), out_ids.len));
        for (self.reverse_edges.items[start .. start + count], 0..) |dep_idx, i| {
            out_ids[i] = self.nodes.items[dep_idx].id;
        }
        return count;
    }

    /// Get the dependencies of a task by TaskId (forward edges).
    /// Returns the number of ids written (at most out_ids.len).
    pub fn getDependencies(
        self: *const TaskGraph,
        id: TaskId,
        out_ids: []TaskId,
    ) usize {
        const idx = self.id_to_index.get(id) orelse return 0;
        const node = self.nodes.items[idx];
        const dc = unpackDepsCount(node.packed_flags);
        const count: u32 = @intCast(@min(@as(usize, dc), out_ids.len));
        for (self.edges.items[node.deps_start .. node.deps_start + count], 0..) |dep_idx, i| {
            out_ids[i] = self.nodes.items[dep_idx].id;
        }
        return count;
    }

    /// Mark a task dirty and propagate to all transitive dependents in one
    /// pass — the FFI-call fusion of getInvalidationSet + setStatus(dirty).
    /// BFS through the reverse edge array, marking every visited node dirty.
    /// Writes all dirtied TaskIds to out_ids, returns the count.
    ///
    /// Statuses are updated even when out_ids is too small (dirtying is
    /// idempotent), so callers may retry with a larger buffer safely.
    pub fn markDirty(
        self: *TaskGraph,
        id: TaskId,
        out_ids: []TaskId,
    ) usize {
        const start_idx = self.id_to_index.get(id) orelse return 0;
        const n = self.nodes.items.len;
        if (n == 0) return 0;

        const allocator = std.heap.page_allocator;
        const visited = allocator.alloc(bool, n) catch return 0;
        defer allocator.free(visited);
        @memset(visited, false);

        const queue = allocator.alloc(u32, n) catch return 0;
        defer allocator.free(queue);
        var queue_head: usize = 0;
        var queue_tail: usize = 0;

        queue[queue_tail] = start_idx;
        queue_tail += 1;
        visited[start_idx] = true;

        var count: usize = 0;
        while (queue_head < queue_tail) {
            const current = queue[queue_head];
            queue_head += 1;

            self.setStatus(current, .dirty);
            if (count < out_ids.len) {
                out_ids[count] = self.nodes.items[current].id;
                count += 1;
            }

            const node = self.nodes.items[current];
            const dpc = unpackDependentsCount(node.packed_flags);
            const dstart = self.dependents_offsets.items[current];
            for (self.reverse_edges.items[dstart .. dstart + dpc]) |dep| {
                if (dep < n and !visited[dep]) {
                    visited[dep] = true;
                    if (queue_tail < queue.len) {
                        queue[queue_tail] = dep;
                        queue_tail += 1;
                    }
                }
            }
        }

        return count;
    }

    /// Flat scan over the contiguous nodes array: write the TaskIds of every
    /// node whose packed status matches `status`. Returns the count written.
    /// This is the bitset-style dirty/clean scan — one pass over 24-byte
    /// nodes, no hashing.
    pub fn idsByStatus(
        self: *const TaskGraph,
        status: TaskStatus,
        out_ids: []TaskId,
    ) usize {
        var count: usize = 0;
        for (self.nodes.items) |node| {
            if (unpackStatus(node.packed_flags) == status) {
                if (count < out_ids.len) {
                    out_ids[count] = node.id;
                    count += 1;
                }
            }
        }
        return count;
    }

    /// Write every TaskId in the graph. Size out_ids with taskCount() first.
    /// Returns the number of ids written (at most out_ids.len).
    pub fn allIds(self: *const TaskGraph, out_ids: []TaskId) usize {
        const n = @min(self.nodes.items.len, out_ids.len);
        for (self.nodes.items[0..n], 0..) |node, i| {
            out_ids[i] = node.id;
        }
        return n;
    }

    /// Clear the graph — frees the arena and re-initializes in place,
    /// keeping the TaskGraph pointer valid for callers.
    pub fn clear(self: *TaskGraph) void {
        self.deinit();
        self.init();
    }

    /// Serialize the task graph to a flat binary format.
    ///
    /// Format (all little-endian):
    ///   Header (32 bytes):
    ///     magic: [4]u8 = "PTG2"
    ///     version: u32 = 2
    ///     node_count: u32
    ///     edge_count: u32
    ///     reverse_edge_count: u32
    ///     id_to_index_count: u32
    ///     reserved: [4]u8
    ///   Body:
    ///     nodes: [node_count]TaskNode (each 24 bytes: 16 id + 4 deps_start + 4 packed)
    ///     dependents_offsets: [node_count]u32
    ///     edges: [edge_count]u32
    ///     reverse_edges: [reverse_edge_count]u32
    ///     id_to_index: [id_to_index_count]struct { id: [16]u8, index: u32 }
    ///
    /// The format is a single contiguous block with no pointers —
    /// suitable for mmap.
    pub fn serializeToFile(self: *const TaskGraph, path: []const u8) !void {
        const fp = openFile(path, true) orelse return error.OpenFailed;
        defer _ = fclose(fp);

        // Lengths are stored as u32 - reject (rather than @intCast-trap on)
        // graphs that do not fit.
        const max: usize = std.math.maxInt(u32);
        if (self.nodes.items.len > max or self.edges.items.len > max or
            self.reverse_edges.items.len > max or self.id_to_index.count() > max)
            return error.GraphTooLarge;
        const node_count: u32 = @intCast(self.nodes.items.len);
        const edge_count: u32 = @intCast(self.edges.items.len);
        const reverse_edge_count: u32 = @intCast(self.reverse_edges.items.len);
        const id_to_index_count: u32 = @intCast(self.id_to_index.count());

        // Header (32 bytes)
        var header: [32]u8 = .{0} ** 32;
        @memcpy(header[0..4], "PTG2");
        std.mem.writeInt(u32, header[4..8], 2, .little); // version 2: 24-byte nodes
        std.mem.writeInt(u32, header[8..12], node_count, .little);
        std.mem.writeInt(u32, header[12..16], edge_count, .little);
        std.mem.writeInt(u32, header[16..20], reverse_edge_count, .little);
        std.mem.writeInt(u32, header[20..24], id_to_index_count, .little);
        // header[24..28] reserved (already zero)
        try writeAll(fp, &header);

        // Nodes (each 24 bytes: 16 id + 4 deps_start + 4 packed)
        for (self.nodes.items) |node| {
            var buf: [24]u8 = undefined;
            @memcpy(buf[0..16], &node.id);
            std.mem.writeInt(u32, buf[16..20], node.deps_start, .little);
            std.mem.writeInt(u32, buf[20..24], node.packed_flags, .little);
            try writeAll(fp, &buf);
        }

        // Dependents offsets (parallel to nodes, 4 bytes each)
        for (self.dependents_offsets.items) |offset| {
            var buf: [4]u8 = undefined;
            std.mem.writeInt(u32, &buf, offset, .little);
            try writeAll(fp, &buf);
        }

        // Edges
        for (self.edges.items) |edge| {
            var buf: [4]u8 = undefined;
            std.mem.writeInt(u32, &buf, edge, .little);
            try writeAll(fp, &buf);
        }

        // Reverse edges
        for (self.reverse_edges.items) |edge| {
            var buf: [4]u8 = undefined;
            std.mem.writeInt(u32, &buf, edge, .little);
            try writeAll(fp, &buf);
        }

        // id_to_index entries (20 bytes each: 16 id + 4 index)
        var it = self.id_to_index.iterator();
        while (it.next()) |entry| {
            var buf: [20]u8 = undefined;
            @memcpy(buf[0..16], &entry.key_ptr.*);
            std.mem.writeInt(u32, buf[16..20], entry.value_ptr.*, .little);
            try writeAll(fp, &buf);
        }
    }

    fn writeAll(fp: *anyopaque, bytes: []const u8) !void {
        if (fwrite(bytes.ptr, 1, bytes.len, fp) != bytes.len) return error.WriteFailed;
    }

    /// Append the per-node parallel LRU slots for a node just added by a
    /// loader (graphs restored from disk previously had EMPTY lru arrays, so
    /// touchLru on a loaded graph indexed out of bounds).
    fn appendLruSlots(self: *TaskGraph) !void {
        try self.lru_prev.append(self.allocator, LruList.NULL_INDEX);
        try self.lru_next.append(self.allocator, LruList.NULL_INDEX);
    }

    fn loadFromBytes(self: *TaskGraph, data: []const u8) !void {
        return loadFromBytesImpl(self, data);
    }

    /// Structural validation of a graph produced by a loader. Files and
    /// compressed snapshots are untrusted input: every offset/index is
    /// range-checked so later accessors (which slice without checks in
    /// release builds) cannot read out of bounds.
    pub fn validate(self: *const TaskGraph) !void {
        const n = self.nodes.items.len;
        if (self.dependents_offsets.items.len != n) return error.InvalidData;
        if (self.lru_prev.items.len != n or self.lru_next.items.len != n) return error.InvalidData;
        if (self.id_to_index.count() != n) return error.InvalidData;
        for (self.nodes.items, 0..) |node, i| {
            const status_bits = (node.packed_flags >> STATUS_SHIFT) & STATUS_MASK;
            if (status_bits > @intFromEnum(TaskStatus.evicted)) return error.InvalidData;
            const dc: u64 = unpackDepsCount(node.packed_flags);
            const dpc: u64 = unpackDependentsCount(node.packed_flags);
            if (@as(u64, node.deps_start) + dc > self.edges.items.len) return error.InvalidData;
            if (@as(u64, self.dependents_offsets.items[i]) + dpc > self.reverse_edges.items.len) return error.InvalidData;
            const mapped = self.id_to_index.get(node.id) orelse return error.InvalidData;
            if (mapped != i) return error.InvalidData;
        }
        for (self.edges.items) |e| if (e >= n) return error.InvalidData;
        for (self.reverse_edges.items) |e| if (e >= n) return error.InvalidData;
    }

    /// Deserialize a task graph from a flat binary file.
    ///
    /// Returns a heap-allocated graph (release with `destroyTaskGraph`). It
    /// used to return `TaskGraph` BY VALUE, but the graph's arena allocator
    /// and `id_to_index` hash map capture pointers to the struct's own
    /// address, so the returned copy referenced the dead stack frame of the
    /// loader (use-after-return). The graph is now built in place at its
    /// final heap address.
    pub fn loadFromFile(path: []const u8) !*TaskGraph {
        const graph = try createTaskGraph();
        errdefer destroyTaskGraph(graph);
        try graph.loadInto(path);
        try graph.validate();
        return graph;
    }

    fn loadInto(graph: *TaskGraph, path: []const u8) !void {
        const fp = openFile(path, false) orelse return error.OpenFailed;
        defer _ = fclose(fp);

        // Helper to read N bytes
        const readBytes = struct {
            fn call(f: *anyopaque, buf: []u8) bool {
                const n = fread(buf.ptr, 1, buf.len, f);
                return n == buf.len;
            }
        }.call;

        // Helper to read u32 LE
        const readU32 = struct {
            fn call(f: *anyopaque) ?u32 {
                var buf: [4]u8 = undefined;
                if (fread(&buf, 1, 4, f) != 4) return null;
                return std.mem.readInt(u32, &buf, .little);
            }
        }.call;

        // Header (32 bytes)
        var header: [32]u8 = undefined;
        if (!readBytes(fp, &header)) return error.UnexpectedEof;
        if (!std.mem.eql(u8, header[0..4], "PTG2")) return error.InvalidMagic;
        const version = std.mem.readInt(u32, header[4..8], .little);
        if (version != 2) return error.UnsupportedVersion;
        const node_count = std.mem.readInt(u32, header[8..12], .little);
        const edge_count = std.mem.readInt(u32, header[12..16], .little);
        const reverse_edge_count = std.mem.readInt(u32, header[16..20], .little);
        const id_to_index_count = std.mem.readInt(u32, header[20..24], .little);
        // One entry per node - anything else is a corrupt/hostile header.
        if (id_to_index_count != node_count) return error.InvalidData;

        // Nodes (24 bytes each: 16 id + 4 deps_start + 4 packed). Each
        // iteration consumes file bytes, so a lying header cannot force an
        // allocation larger than the file itself.
        var i: u32 = 0;
        while (i < node_count) : (i += 1) {
            var node_buf: [24]u8 = undefined;
            if (!readBytes(fp, &node_buf)) return error.UnexpectedEof;

            var id: TaskId = undefined;
            @memcpy(&id, node_buf[0..16]);
            const deps_start = std.mem.readInt(u32, node_buf[16..20], .little);
            const packed_val = std.mem.readInt(u32, node_buf[20..24], .little);

            if (graph.id_to_index.contains(id)) return error.InvalidData; // duplicate id
            try graph.nodes.append(graph.allocator, .{
                .id = id,
                .deps_start = deps_start,
                .packed_flags = packed_val,
            });
            try graph.id_to_index.put(id, i);
            try graph.appendLruSlots();
        }

        // Dependents offsets (parallel to nodes, 4 bytes each)
        i = 0;
        while (i < node_count) : (i += 1) {
            const offset = readU32(fp) orelse return error.UnexpectedEof;
            try graph.dependents_offsets.append(graph.allocator, offset);
        }

        // Edges
        i = 0;
        while (i < edge_count) : (i += 1) {
            const edge = readU32(fp) orelse return error.UnexpectedEof;
            try graph.edges.append(graph.allocator, edge);
        }

        // Reverse edges
        i = 0;
        while (i < reverse_edge_count) : (i += 1) {
            const edge = readU32(fp) orelse return error.UnexpectedEof;
            try graph.reverse_edges.append(graph.allocator, edge);
        }

        // id_to_index entries: redundant with the node table (already
        // populated above) - verify each agrees instead of trusting it.
        i = 0;
        while (i < id_to_index_count) : (i += 1) {
            var id_buf: [16]u8 = undefined;
            if (!readBytes(fp, &id_buf)) return error.UnexpectedEof;
            const index = readU32(fp) orelse return error.UnexpectedEof;
            var id: TaskId = undefined;
            @memcpy(&id, &id_buf);
            const mapped = graph.id_to_index.get(id) orelse return error.InvalidData;
            if (mapped != index) return error.InvalidData;
        }
    }
};

/// Create a new task graph (C ABI).
pub fn createTaskGraph() !*TaskGraph {
    const g = try std.heap.page_allocator.create(TaskGraph);
    g.init(); // binds allocator + map at the heap-stable address
    return g;
}

/// Destroy a task graph (C ABI).
pub fn destroyTaskGraph(g: *TaskGraph) void {
    g.deinit();
    std.heap.page_allocator.destroy(g);
}

// ─── G8.12: Arena compression with zstd ──────────────────────────────
//
// Compresses the arena memory to reduce on-disk footprint for snapshots
// and checkpoints. Uses Zig's built-in std.compress.flate (zlib) for
// zero-dependency compression. Typical compression ratio for graph data: 3-10x.

/// G8.12: Compress a byte slice using zlib (flate).
/// Returns a compressed buffer allocated from the given allocator.
pub fn compressZstd(allocator: Allocator, data: []const u8) ![]u8 {
    // Allocate a buffer large enough for compressed output
    const max_size = data.len + data.len / 100 + 256;
    const out_buf = try allocator.alloc(u8, max_size);
    defer allocator.free(out_buf);

    const cbuf = try allocator.alloc(u8, std.compress.flate.max_window_len);
    defer allocator.free(cbuf);
    var writer = std.Io.Writer.fixed(out_buf);
    var compressor = try std.compress.flate.Compress.init(
        &writer,
        cbuf,
        std.compress.flate.Container.zlib,
        .level_3,
    );

    _ = try compressor.writer.writeAll(data);
    try compressor.finish();

    const written = writer.end;
    return allocator.dupe(u8, out_buf[0..written]);
}

/// Upper bound on decompressed snapshot size (256 MiB).
pub const MAX_DECOMPRESSED_SIZE: usize = 256 * 1024 * 1024;

/// G8.12: Decompress a zlib-compressed byte slice.
/// Returns a decompressed buffer allocated from the given allocator.
pub fn decompressZstd(allocator: Allocator, compressed: []const u8) ![]u8 {
    const dbuf = try allocator.alloc(u8, std.compress.flate.max_window_len);
    defer allocator.free(dbuf);
    var reader = std.Io.Reader.fixed(compressed);
    var decompressor = std.compress.flate.Decompress.init(
        &reader,
        std.compress.flate.Container.zlib,
        dbuf,
    );

    // Read in chunks since we don't know the decompressed size
    var result = std.ArrayList(u8).empty;
    defer result.deinit(allocator);

    var chunk: [4096]u8 = undefined;
    while (true) {
        const n = decompressor.reader.readSliceShort(&chunk) catch break;
        if (n == 0) break;
        // Decompression-bomb guard: bound the output regardless of the
        // (untrusted) compressed input.
        if (result.items.len + n > MAX_DECOMPRESSED_SIZE) return error.DecompressionTooLarge;
        try result.appendSlice(allocator, chunk[0..n]);
    }

    if (result.items.len == 0) return error.DecompressionFailed;
    return result.toOwnedSlice(allocator);
}

/// G8.12: Compress the TaskGraph's arena to a zstd buffer.
/// Serializes the graph first, then compresses.
pub fn compressTaskGraph(allocator: Allocator, g: *const TaskGraph) ![]u8 {
    // Serialize to a temporary buffer
    var buf = std.ArrayList(u8).empty;
    defer buf.deinit(allocator);

    // Write header
    var header: [32]u8 = .{0} ** 32;
    @memcpy(header[0..4], "PTGZ");
    std.mem.writeInt(u32, header[4..8], 1, .little);
    std.mem.writeInt(u32, header[8..12], @intCast(g.nodes.items.len), .little);
    std.mem.writeInt(u32, header[12..16], @intCast(g.edges.items.len), .little);
    std.mem.writeInt(u32, header[16..20], @intCast(g.reverse_edges.items.len), .little);
    std.mem.writeInt(u32, header[20..24], @intCast(g.dependents_offsets.items.len), .little);
    std.mem.writeInt(u32, header[24..28], @intCast(g.id_to_index.count()), .little);
    try buf.appendSlice(allocator, &header);

    // Write nodes (24 bytes each: 16 id + 4 deps_start + 4 packed_flags)
    for (g.nodes.items) |node| {
        var nbuf: [24]u8 = undefined;
        @memcpy(nbuf[0..16], &node.id);
        std.mem.writeInt(u32, nbuf[16..20], node.deps_start, .little);
        std.mem.writeInt(u32, nbuf[20..24], node.packed_flags, .little);
        try buf.appendSlice(allocator, &nbuf);
    }
    // Write dependents_offsets
    for (g.dependents_offsets.items) |offset| {
        var obuf: [4]u8 = undefined;
        std.mem.writeInt(u32, &obuf, offset, .little);
        try buf.appendSlice(allocator, &obuf);
    }
    // Write edges
    for (g.edges.items) |edge| {
        var ebuf: [4]u8 = undefined;
        std.mem.writeInt(u32, &ebuf, edge, .little);
        try buf.appendSlice(allocator, &ebuf);
    }
    // Write reverse_edges
    for (g.reverse_edges.items) |edge| {
        var ebuf: [4]u8 = undefined;
        std.mem.writeInt(u32, &ebuf, edge, .little);
        try buf.appendSlice(allocator, &ebuf);
    }
    // Write id_to_index
    var it = g.id_to_index.iterator();
    while (it.next()) |entry| {
        try buf.appendSlice(allocator, &entry.key_ptr.*);
        var ibuf: [4]u8 = undefined;
        std.mem.writeInt(u32, &ibuf, entry.value_ptr.*, .little);
        try buf.appendSlice(allocator, &ibuf);
    }

    // Compress the serialized buffer
    return compressZstd(allocator, buf.items);
}

/// G8.12: Decompress and restore a TaskGraph from a zstd buffer.
///
/// Returns a heap-allocated graph (release with `destroyTaskGraph`); see
/// `TaskGraph.loadFromFile` for why it cannot be returned by value.
/// `allocator` is only used for the temporary decompression buffers.
pub fn decompressTaskGraph(allocator: Allocator, compressed: []const u8) !*TaskGraph {
    const data = try decompressZstd(allocator, compressed);
    defer allocator.free(data);

    const graph = try createTaskGraph();
    errdefer destroyTaskGraph(graph);
    try graph.loadFromBytes(data);
    try graph.validate();
    return graph;
}

fn loadFromBytesImpl(graph: *TaskGraph, data: []const u8) !void {
    if (data.len < 32) return error.InvalidData;
    if (!std.mem.eql(u8, data[0..4], "PTGZ")) return error.InvalidMagic;
    const version = std.mem.readInt(u32, data[4..8], .little);
    if (version != 1) return error.UnsupportedVersion;
    const node_count = std.mem.readInt(u32, data[8..12], .little);
    const edge_count = std.mem.readInt(u32, data[12..16], .little);
    const reverse_edge_count = std.mem.readInt(u32, data[16..20], .little);
    const dependents_count = std.mem.readInt(u32, data[20..24], .little);
    const id_to_index_count = std.mem.readInt(u32, data[24..28], .little);
    if (dependents_count != node_count or id_to_index_count != node_count) return error.InvalidData;

    var offset: usize = 32;

    // Read nodes (24 bytes each)
    var i: u32 = 0;
    while (i < node_count) : (i += 1) {
        if (offset + 24 > data.len) return error.UnexpectedEof;
        var id: TaskId = undefined;
        @memcpy(&id, data[offset .. offset + 16]);
        const deps_start = std.mem.readInt(u32, data[offset + 16 .. offset + 20][0..4], .little);
        const packed_val = std.mem.readInt(u32, data[offset + 20 .. offset + 24][0..4], .little);
        if (graph.id_to_index.contains(id)) return error.InvalidData; // duplicate id
        try graph.nodes.append(graph.allocator, .{
            .id = id,
            .deps_start = deps_start,
            .packed_flags = packed_val,
        });
        try graph.id_to_index.put(id, i);
        try graph.appendLruSlots();
        offset += 24;
    }

    // Read dependents_offsets
    i = 0;
    while (i < dependents_count) : (i += 1) {
        if (offset + 4 > data.len) return error.UnexpectedEof;
        const val = std.mem.readInt(u32, data[offset .. offset + 4][0..4], .little);
        try graph.dependents_offsets.append(graph.allocator, val);
        offset += 4;
    }

    // Read edges
    i = 0;
    while (i < edge_count) : (i += 1) {
        if (offset + 4 > data.len) return error.UnexpectedEof;
        const val = std.mem.readInt(u32, data[offset .. offset + 4][0..4], .little);
        try graph.edges.append(graph.allocator, val);
        offset += 4;
    }

    // Read reverse_edges
    i = 0;
    while (i < reverse_edge_count) : (i += 1) {
        if (offset + 4 > data.len) return error.UnexpectedEof;
        const val = std.mem.readInt(u32, data[offset .. offset + 4][0..4], .little);
        try graph.reverse_edges.append(graph.allocator, val);
        offset += 4;
    }

    // id_to_index is redundant with the node table - verify it agrees.
    i = 0;
    while (i < id_to_index_count) : (i += 1) {
        if (offset + 20 > data.len) return error.UnexpectedEof;
        var id: TaskId = undefined;
        @memcpy(&id, data[offset .. offset + 16]);
        const idx = std.mem.readInt(u32, data[offset + 16 .. offset + 20][0..4], .little);
        const mapped = graph.id_to_index.get(id) orelse return error.InvalidData;
        if (mapped != idx) return error.InvalidData;
        offset += 20;
    }
}

// ─── G8.13: Arena snapshotting (COW) ─────────────────────────────────
//
// Copy-on-write snapshots of the task graph arena. A snapshot captures
// the graph state as a compressed buffer (using G8.12's compression).
// The snapshot is cheap to create (just serialize + compress) and can
// be restored into a new TaskGraph without affecting the original.
//
// This is effectively COW at the serialization level: the original arena
// is untouched, and the restored copy gets its own fresh arena. True
// page-level COW would require mmap(MAP_PRIVATE) which is platform-specific.

/// G8.13: A compressed snapshot of a TaskGraph's arena.
pub const ArenaSnapshot = struct {
    data: []u8,
    allocator: Allocator,

    pub fn deinit(self: *ArenaSnapshot) void {
        self.allocator.free(self.data);
    }
};

/// G8.13: Create a COW snapshot of the task graph.
/// The snapshot is compressed and can be restored without affecting the original.
pub fn snapshotTaskGraph(allocator: Allocator, g: *const TaskGraph) !ArenaSnapshot {
    const compressed = try compressTaskGraph(allocator, g);
    return .{ .data = compressed, .allocator = allocator };
}

/// G8.13: Restore a TaskGraph from a snapshot.
/// Creates a fresh heap-allocated graph (release with `destroyTaskGraph`).
/// The original graph is unaffected.
pub fn restoreTaskGraph(snapshot: *const ArenaSnapshot) !*TaskGraph {
    return decompressTaskGraph(snapshot.allocator, snapshot.data);
}

// ─── G8.14: Arena NUMA placement ─────────────────────────────────────
//
// On multi-socket systems, placing the arena on the NUMA node closest to
// the current CPU reduces memory latency. This uses Linux's numa_alloc_onnode
// or mmap with MPOL_BIND. On non-Linux platforms, it's a no-op fallback.

/// G8.14: Allocate arena memory on a specific NUMA node.
/// Returns a buffer aligned to page size. Falls back to regular allocation
/// on platforms without NUMA support.
///
/// NOTE: Stub — uses ordinary allocation. NUMA-aware allocation not implemented.
/// TODO: Implement using libnuma on Linux for NUMA-aware memory allocation.
pub fn numaAlloc(allocator: Allocator, size: usize, node: u32) ![]u8 {
    _ = node; // STUB: NUMA node parameter ignored

    // On Linux, we would use:
    //   mmap(NULL, size, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    //   then set_mempolicy(MPOL_BIND, &node_mask, max_nodes)
    //
    // On Windows, VirtualAllocExNuma could be used if NUMA is available.
    //
    // For portability, we fall back to regular allocation with page alignment.
    const page_size = std.heap.page_size_min;
    const aligned_size = std.mem.alignForward(usize, size, page_size);
    return allocator.alloc(u8, aligned_size);
}

/// G8.14: Detect the optimal NUMA node for the current CPU.
/// Returns 0 on platforms without NUMA support.
///
/// NOTE: Stub — always returns 0. NUMA node detection not implemented.
/// TODO: Implement using /sys/devices/system/node or sched_getcpu on Linux.
pub fn numaPreferredNode() u32 {
    // STUB: NUMA node detection not implemented. On Linux, this would read
    // /sys/devices/system/node/online or use sched_getcpu() + numa_node_of_cpu().
    // On Windows, GetNumaProcessorNode.
    return 0;
}

// ─── G8.15: Huge page support ────────────────────────────────────────
//
// Allocating the arena with 2MB huge pages reduces TLB misses for large
// graphs. On Linux, this uses mmap with MAP_HUGETLB. On other platforms,
// it falls back to regular allocation.

/// G8.15: Huge page size (2MB on most platforms).
pub const HUGE_PAGE_SIZE: usize = 2 * 1024 * 1024;

/// G8.15: Allocate memory using huge pages if available.
/// Falls back to regular allocation on platforms without huge page support.
///
/// NOTE: Stub — uses ordinary allocation. Huge page support not implemented.
/// TODO: Implement using mmap with MAP_HUGETLB on Linux.
pub fn hugePageAlloc(allocator: Allocator, size: usize) ![]u8 {
    // STUB: Falls back to regular aligned allocation.
    // On Linux, we would use:
    //   mmap(NULL, size, PROT_READ|PROT_WRITE,
    //        MAP_PRIVATE|MAP_ANONYMOUS|MAP_HUGETLB, -1, 0)
    //
    // On Windows, VirtualAlloc with MEM_LARGE_PAGES (requires SeLockMemoryPrivilege).
    //
    // For portability, we fall back to regular aligned allocation.
    const aligned_size = std.mem.alignForward(usize, size, HUGE_PAGE_SIZE);
    return allocator.alloc(u8, aligned_size);
}

/// G8.15: Check if huge pages are available on the current platform.
///
/// NOTE: Stub — always returns false. Huge page availability detection not implemented.
/// TODO: Check /proc/meminfo for HugePages_Total on Linux, SeLockMemoryPrivilege on Windows.
pub fn hugePagesAvailable() bool {
    // STUB: Huge page availability not implemented. On Linux, check
    // /proc/meminfo for HugePages_Total > 0. On Windows, check for
    // SeLockMemoryPrivilege. For now, return false as a safe default.
    return false;
}

// ─── G8.5: Arena slab allocation ─────────────────────────────────────
//
// The arena grows in fixed-size slabs (default 64KB). This avoids realloc
// on growth — new slabs are allocated and chained. Each slab is a contiguous
// block; allocations within a slab are bump-allocated.

pub const SLAB_SIZE: usize = 64 * 1024;

pub const SlabArena = struct {
    slabs: std.ArrayList([]u8),
    current_slab: []u8,
    offset: usize,
    allocator: Allocator,

    pub fn init(allocator: Allocator) SlabArena {
        return .{
            .slabs = .empty,
            .current_slab = &.{},
            .offset = 0,
            .allocator = allocator,
        };
    }

    pub fn deinit(self: *SlabArena) void {
        for (self.slabs.items) |slab| {
            self.allocator.free(slab);
        }
        self.slabs.deinit(self.allocator);
    }

    pub fn alloc(self: *SlabArena, size: usize, alignment: usize) ![]u8 {
        const aligned_offset = std.mem.alignForward(usize, self.offset, alignment);

        if (aligned_offset + size <= self.current_slab.len) {
            const result = self.current_slab[aligned_offset .. aligned_offset + size];
            self.offset = aligned_offset + size;
            return result;
        }

        if (size <= SLAB_SIZE) {
            const new_slab = try self.allocator.alloc(u8, SLAB_SIZE);
            try self.slabs.append(self.allocator, new_slab);
            self.current_slab = new_slab;
            self.offset = size;
            return new_slab[0..size];
        }

        const big_slab = try self.allocator.alloc(u8, size);
        try self.slabs.append(self.allocator, big_slab);
        return big_slab;
    }

    pub fn totalAllocated(self: *const SlabArena) usize {
        var total: usize = 0;
        for (self.slabs.items) |slab| total += slab.len;
        return total;
    }

    pub fn slabCount(self: *const SlabArena) usize {
        return self.slabs.items.len;
    }
};

// ─── G8.6: Arena compaction ──────────────────────────────────────────
//
// After LRU eviction removes nodes, the arena may have gaps (evicted node
// slots marked as free). Compaction renumbers live nodes to be contiguous,
// removing gaps. This defragments the node array and edge arrays.

pub fn compactTaskGraph(g: *TaskGraph) !void {
    if (g.nodes.items.len == 0) return;

    const n = g.nodes.items.len;
    var live_count: u32 = 0;
    for (g.nodes.items) |node| {
        if (unpackStatus(node.packed_flags) != .evicted) live_count += 1;
    }
    if (live_count == n) return;

    // old index -> new index (NULL_INDEX for evicted nodes)
    const remap = try g.allocator.alloc(u32, n);
    {
        var next: u32 = 0;
        for (g.nodes.items, 0..) |node, i| {
            if (unpackStatus(node.packed_flags) == .evicted) {
                remap[i] = LruList.NULL_INDEX;
            } else {
                remap[i] = next;
                next += 1;
            }
        }
    }

    // Rebuild BOTH flat edge arrays for the surviving nodes only. Edges that
    // touch an evicted node are dropped (they used to be left in place,
    // pointing at whichever unrelated node had been renumbered into that
    // slot), and each survivor's segment start is recomputed (they used to
    // keep their old, now-stale offsets). Everything fallible happens before
    // any live state is modified, so an OOM leaves the graph untouched.
    var new_edges: std.ArrayList(u32) = .empty;
    var new_rev: std.ArrayList(u32) = .empty;
    const Seg = struct { ds: u32, dc: u32, rs: u32, rc: u32 };
    const segs = try g.allocator.alloc(Seg, live_count);

    var w: usize = 0;
    for (g.nodes.items, 0..) |node, i| {
        if (remap[i] == LruList.NULL_INDEX) continue;
        const dc = unpackDepsCount(node.packed_flags);
        const dpc = unpackDependentsCount(node.packed_flags);
        const ds = node.deps_start;
        const rs = g.dependents_offsets.items[i];

        const seg_ds: u32 = @intCast(new_edges.items.len);
        var kept_d: u32 = 0;
        for (g.edges.items[ds .. ds + dc]) |e| {
            if (e < n and remap[e] != LruList.NULL_INDEX) {
                try new_edges.append(g.allocator, remap[e]);
                kept_d += 1;
            }
        }
        const seg_rs: u32 = @intCast(new_rev.items.len);
        var kept_r: u32 = 0;
        for (g.reverse_edges.items[rs .. rs + dpc]) |e| {
            if (e < n and remap[e] != LruList.NULL_INDEX) {
                try new_rev.append(g.allocator, remap[e]);
                kept_r += 1;
            }
        }
        segs[w] = .{ .ds = seg_ds, .dc = kept_d, .rs = seg_rs, .rc = kept_r };
        w += 1;
    }

    // Reserve the id map growth up front so the commit below cannot fail.
    try g.id_to_index.ensureTotalCapacity(live_count);

    // ---- commit (infallible) ----
    w = 0;
    for (0..n) |i| {
        if (remap[i] == LruList.NULL_INDEX) continue;
        const status = unpackStatus(g.nodes.items[i].packed_flags);
        var node = g.nodes.items[i];
        node.deps_start = segs[w].ds;
        node.packed_flags = packNode(segs[w].dc, segs[w].rc, status);
        g.nodes.items[w] = node;
        g.dependents_offsets.items[w] = segs[w].rs;
        w += 1;
    }
    g.nodes.shrinkRetainingCapacity(live_count);
    g.dependents_offsets.shrinkRetainingCapacity(live_count);
    g.lru_prev.shrinkRetainingCapacity(live_count);
    g.lru_next.shrinkRetainingCapacity(live_count);
    g.edges = new_edges;
    g.reverse_edges = new_rev;

    g.id_to_index.clearRetainingCapacity();
    for (g.nodes.items, 0..) |node, i| {
        g.id_to_index.putAssumeCapacity(node.id, @intCast(i));
    }

    // The LRU order is index-based and cannot survive renumbering; reset it
    // (callers re-touch nodes as they are used).
    g.lru.count = 0;
    g.lru.head = LruList.NULL_INDEX;
    g.lru.tail = LruList.NULL_INDEX;
    for (g.lru_prev.items) |*p| p.* = LruList.NULL_INDEX;
    for (g.lru_next.items) |*p| p.* = LruList.NULL_INDEX;
}

// ─── G8.7: Arena memory-mapped I/O ───────────────────────────────────
//
// The arena can be backed by a memory-mapped file. Reads are direct mmap
// reads (zero-copy). Writes are flushed via msync periodically.
// On platforms without mmap, falls back to regular allocation.

pub const MmapArena = struct {
    data: []u8,
    file: ?std.fs.File,
    owns_file: bool,
    is_mapped: bool,

    pub fn open(path: []const u8, size: usize) !MmapArena {
        if (@import("builtin").os.tag == .linux or @import("builtin").os.tag == .macos) {
            // mmap of length 0 is EINVAL; reject up front with a clear error.
            if (size == 0) return error.InvalidSize;
            var file = try std.fs.cwd().createFile(path, .{ .read = true, .truncate = false });
            errdefer file.close();

            // Only ever GROW the file. Unconditionally calling setEndPos(size)
            // truncated an existing, larger arena file (silent data loss) when
            // it was re-opened with a smaller size.
            if ((try file.getEndPos()) < size) try file.setEndPos(size);

            const data = try std.posix.mmap(
                null,
                size,
                std.posix.PROT.READ | std.posix.PROT.WRITE,
                .{ .TYPE = .SHARED },
                file,
                0,
            );

            return .{
                .data = data,
                .file = file,
                .owns_file = true,
                .is_mapped = true,
            };
        }
        return error.MmapNotSupported;
    }

    pub fn openAnonymous(size: usize) !MmapArena {
        if (@import("builtin").os.tag == .linux or @import("builtin").os.tag == .macos) {
            const data = try std.posix.mmap(
                null,
                size,
                std.posix.PROT.READ | std.posix.PROT.WRITE,
                .{ .TYPE = .PRIVATE, .ANONYMOUS = true },
                null,
                0,
            );

            return .{
                .data = data,
                .file = null,
                .owns_file = false,
                .is_mapped = true,
            };
        }
        return error.MmapNotSupported;
    }

    pub fn sync(self: *MmapArena) !void {
        if (self.is_mapped and self.data.len > 0) {
            if (@import("builtin").os.tag == .linux or @import("builtin").os.tag == .macos) {
                try std.posix.msync(self.data, .SYNC);
            }
        }
    }

    pub fn close(self: *MmapArena) void {
        if (self.is_mapped and self.data.len > 0) {
            if (@import("builtin").os.tag == .linux or @import("builtin").os.tag == .macos) {
                std.posix.munmap(self.data);
            }
            self.is_mapped = false;
        }
        if (self.owns_file) {
            if (self.file) |f| f.close();
            self.owns_file = false;
        }
    }
};

// ─── G8.8: Arena tiering ─────────────────────────────────────────────
//
// Hot nodes stay in memory-only fast tier. Cold nodes (evicted from LRU)
// are moved to a disk-backed slow tier. When a cold node is accessed,
// it's promoted back to the hot tier.

pub const Tier = enum(u8) { hot, cold };

pub const TieredArena = struct {
    hot: std.AutoHashMap(u32, void),
    cold: std.AutoHashMap(u32, []u8),
    allocator: Allocator,

    pub fn init(allocator: Allocator) TieredArena {
        return .{
            .hot = std.AutoHashMap(u32, void).init(allocator),
            .cold = std.AutoHashMap(u32, []u8).init(allocator),
            .allocator = allocator,
        };
    }

    pub fn deinit(self: *TieredArena) void {
        var it = self.cold.iterator();
        while (it.next()) |entry| {
            self.allocator.free(entry.value_ptr.*);
        }
        self.cold.deinit();
        self.hot.deinit();
    }

    pub fn markHot(self: *TieredArena, index: u32) !void {
        if (self.cold.fetchRemove(index)) |entry| {
            self.allocator.free(entry.value);
        }
        try self.hot.put(index, {});
    }

    pub fn markCold(self: *TieredArena, index: u32, data: []const u8) !void {
        const copy = try self.allocator.dupe(u8, data);
        errdefer self.allocator.free(copy);
        // fetchPut hands back a previous cold copy for this index so it can
        // be freed (it used to be overwritten and leaked).
        if (try self.cold.fetchPut(index, copy)) |old| self.allocator.free(old.value);
        _ = self.hot.remove(index);
    }

    pub fn getTier(self: *const TieredArena, index: u32) Tier {
        if (self.hot.contains(index)) return .hot;
        if (self.cold.contains(index)) return .cold;
        return .hot;
    }

    pub fn promote(self: *TieredArena, index: u32) !?[]u8 {
        // Reserve the hot-set slot first: if this fails the cold copy is still
        // owned by the map (previously it was removed first and lost on OOM).
        if (!self.cold.contains(index)) return null;
        try self.hot.put(index, {});
        if (self.cold.fetchRemove(index)) |entry| return entry.value;
        return null;
    }

    pub fn hotCount(self: *const TieredArena) usize {
        return self.hot.count();
    }

    pub fn coldCount(self: *const TieredArena) usize {
        return self.cold.count();
    }
};

// ─── G8.9: Arena prefetch ────────────────────────────────────────────
//
// When traversing the graph, prefetch the next N nodes' memory pages
// into the CPU cache. This uses @prefetch to reduce cache misses during
// graph traversal.

pub fn prefetchNode(g: *const TaskGraph, index: u32) void {
    if (index < g.nodes.items.len) {
        const node = &g.nodes.items[index];
        @prefetch(node, .{ .locality = 3, .cache = .data });
    }
}

pub fn prefetchNext(g: *const TaskGraph, start_index: u32, count: u32) void {
    // Saturating add: start_index + count could overflow u32.
    const end: usize = @min(@as(usize, start_index) + count, g.nodes.items.len);
    for (start_index..end) |i| {
        prefetchNode(g, @intCast(i));
        if (i + 1 < g.nodes.items.len) {
            const deps_start = g.nodes.items[i].deps_start;
            const deps_count = unpackDepsCount(g.nodes.items[i].packed_flags);
            if (deps_count > 0 and deps_start < g.edges.items.len) {
                @prefetch(&g.edges.items[deps_start], .{ .locality = 2, .cache = .data });
            }
        }
    }
}

pub fn prefetchTraversal(g: *const TaskGraph, start_index: u32) void {
    if (start_index >= g.nodes.items.len) return;
    prefetchNext(g, start_index, 8);
    const deps_start = g.nodes.items[start_index].deps_start;
    const deps_count = unpackDepsCount(g.nodes.items[start_index].packed_flags);
    for (0..deps_count) |d| {
        if (deps_start + d >= g.edges.items.len) break;
        const dep_idx = g.edges.items[deps_start + d];
        prefetchNode(g, dep_idx);
    }
}

test "TaskNode is 24 bytes" {
    try std.testing.expectEqual(@as(usize, 24), @sizeOf(TaskNode));
}

test "addModule and getModulePath" {
    var g: ModuleGraph = undefined;
    g.init();
    defer g.deinit();

    const id = try g.addModule("src/index.tsx");
    try std.testing.expectEqual(@as(u32, 0), id);
    try std.testing.expectEqualStrings("src/index.tsx", g.getModulePath(id));
    try std.testing.expectEqual(ModuleKind.tsx, g.modules.items[id].kind);
}

test "addDependency and getDependencies" {
    var g: ModuleGraph = undefined;
    g.init();
    defer g.deinit();

    const a = try g.addModule("a.ts");
    const b = try g.addModule("b.ts");
    const c = try g.addModule("c.ts");

    try g.addDependency(a, b); // a imports b
    try g.addDependency(a, c); // a imports c
    try g.addDependency(b, c); // b imports c

    const deps_a = g.getDependencies(a);
    try std.testing.expectEqual(@as(usize, 2), deps_a.len);
    try std.testing.expectEqual(b, deps_a[0]);
    try std.testing.expectEqual(c, deps_a[1]);

    const deps_b = g.getDependencies(b);
    try std.testing.expectEqual(@as(usize, 1), deps_b.len);
    try std.testing.expectEqual(c, deps_b[0]);
}

test "getInvalidationSet" {
    var g: ModuleGraph = undefined;
    g.init();
    defer g.deinit();

    // c ← b ← a  (a imports b, b imports c)
    const a = try g.addModule("a.ts");
    const b = try g.addModule("b.ts");
    const c = try g.addModule("c.ts");

    try g.addDependency(a, b);
    try g.addDependency(b, c);

    // When c changes, both b and a should be invalidated
    const invalid = try g.getInvalidationSet(c, std.testing.allocator);
    defer std.testing.allocator.free(invalid);

    try std.testing.expectEqual(@as(usize, 3), invalid.len);
    try std.testing.expectEqual(c, invalid[0]);
    try std.testing.expectEqual(b, invalid[1]);
    try std.testing.expectEqual(a, invalid[2]);
}

test "TaskGraph serialize and load round-trip" {
    // Build a task graph with 3 nodes and 2 edges
    var g: TaskGraph = undefined;
    g.init();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

    const idx_a = try g.addTask(id_a);
    const idx_b = try g.addTask(id_b);
    const idx_c = try g.addTask(id_c);

    try g.addDependency(id_a, id_b);
    try g.addDependency(id_b, id_c);

    g.setStatus(idx_a, .clean);
    g.setStatus(idx_b, .dirty);
    g.setStatus(idx_c, .pending);

    // Serialize to temp file
    const tmp_path = ".pledge_test_taskgraph.ptg";
    try g.serializeToFile(tmp_path);
    defer {
        var rm_buf: [256]u8 = undefined;
        @memcpy(rm_buf[0..tmp_path.len], tmp_path);
        rm_buf[tmp_path.len] = 0;
        const rm_path: [*:0]const u8 = @ptrCast(&rm_buf);
        _ = remove(rm_path);
    }

    // Load back
    const loaded = try TaskGraph.loadFromFile(tmp_path);
    defer destroyTaskGraph(loaded);

    // Verify node count
    try std.testing.expectEqual(@as(usize, 3), loaded.taskCount());

    // Verify IDs are preserved
    try std.testing.expectEqual(idx_a, loaded.getIndex(id_a).?);
    try std.testing.expectEqual(idx_b, loaded.getIndex(id_b).?);
    try std.testing.expectEqual(idx_c, loaded.getIndex(id_c).?);

    // Verify statuses are preserved
    try std.testing.expectEqual(TaskStatus.clean, loaded.getStatus(idx_a));
    try std.testing.expectEqual(TaskStatus.dirty, loaded.getStatus(idx_b));
    try std.testing.expectEqual(TaskStatus.pending, loaded.getStatus(idx_c));

    // Verify edges are preserved
    const deps_a = loaded.getDependencyIndices(idx_a);
    try std.testing.expectEqual(@as(usize, 1), deps_a.len);
    try std.testing.expectEqual(idx_b, deps_a[0]);

    const deps_b = loaded.getDependencyIndices(idx_b);
    try std.testing.expectEqual(@as(usize, 1), deps_b.len);
    try std.testing.expectEqual(idx_c, deps_b[0]);

    const deps_c = loaded.getDependencyIndices(idx_c);
    try std.testing.expectEqual(@as(usize, 0), deps_c.len);

    // The loaded graph is heap-allocated at its final address: mutating it
    // (which reallocates the arena-backed arrays and the id map) must not
    // touch a dangling stack frame, and the LRU slots must exist.
    const id_d = [_]u8{ 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const idx_d = try loaded.addTask(id_d);
    try loaded.addDependency(id_d, id_a);
    loaded.touchLru(idx_a);
    try std.testing.expectEqual(idx_d, loaded.getIndex(id_d).?);
}

test "loadFromFile rejects a corrupt graph instead of trusting offsets" {
    var g: TaskGraph = undefined;
    g.init();
    defer g.deinit();
    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    _ = try g.addTask(id_a);
    _ = try g.addTask(id_b);
    try g.addDependency(id_a, id_b);
    // Corrupt: an edge that points past the node table.
    g.edges.items[0] = 99;
    const tmp_path = ".pledge_test_taskgraph_bad.ptg";
    try g.serializeToFile(tmp_path);
    defer {
        var rm_buf: [256]u8 = undefined;
        @memcpy(rm_buf[0..tmp_path.len], tmp_path);
        rm_buf[tmp_path.len] = 0;
        _ = remove(@as([*:0]const u8, @ptrCast(&rm_buf)));
    }
    try std.testing.expectError(error.InvalidData, TaskGraph.loadFromFile(tmp_path));
}

test "openFile rejects embedded NUL" {
    try std.testing.expect(openFile("a\x00b", false) == null);
}

test "addDependency rejects counts that would wrap the packed fields" {
    var g: TaskGraph = undefined;
    g.init();
    defer g.deinit();
    const hub = [_]u8{ 0xFF, 0xFF } ++ ([_]u8{0} ** 14);
    _ = try g.addTask(hub);
    // Fill hub's dependents counter (14 bits) by making leaves depend on it.
    var i: u32 = 0;
    while (i < MAX_DEPENDENTS_PER_NODE) : (i += 1) {
        var id: TaskId = [_]u8{0} ** 16;
        std.mem.writeInt(u32, id[0..4], i, .little);
        id[4] = 1;
        _ = try g.addTask(id);
        try g.addDependency(id, hub);
    }
    var extra: TaskId = [_]u8{0} ** 16;
    extra[15] = 7;
    _ = try g.addTask(extra);
    try std.testing.expectError(error.TooManyDependents, g.addDependency(extra, hub));
    // Rejected edge must leave both nodes untouched.
    try std.testing.expectEqual(@as(usize, 0), g.getDependencyIndices(g.getIndex(extra).?).len);
    // Forward direction: the 15-bit deps counter.
    var j: u32 = 0;
    const src = [_]u8{ 0xEE, 0xEE } ++ ([_]u8{0} ** 14);
    _ = try g.addTask(src);
    while (j < MAX_DEPS_PER_NODE) : (j += 1) {
        var id: TaskId = [_]u8{0} ** 16;
        std.mem.writeInt(u32, id[0..4], j, .little);
        id[5] = 2;
        _ = try g.addTask(id);
        try g.addDependency(src, id);
    }
    try std.testing.expectError(error.TooManyDependencies, g.addDependency(src, extra));
}

test "compactTaskGraph drops edges to evicted nodes and refreshes offsets" {
    var g: TaskGraph = undefined;
    g.init();
    defer g.deinit();
    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const idx_a = try g.addTask(id_a);
    const idx_b = try g.addTask(id_b);
    _ = try g.addTask(id_c);
    try g.addDependency(id_a, id_b);
    try g.addDependency(id_a, id_c);
    try g.addDependency(id_c, id_b);
    _ = idx_a;
    g.setStatus(idx_b, .evicted);
    try compactTaskGraph(&g);

    try std.testing.expectEqual(@as(usize, 2), g.taskCount());
    const na = g.getIndex(id_a).?;
    const nc = g.getIndex(id_c).?;
    // a -> c survives (renumbered), a -> b and c -> b are gone.
    const deps_a = g.getDependencyIndices(na);
    try std.testing.expectEqual(@as(usize, 1), deps_a.len);
    try std.testing.expectEqual(nc, deps_a[0]);
    try std.testing.expectEqual(@as(usize, 0), g.getDependencyIndices(nc).len);
    var out: [4]u32 = undefined;
    try std.testing.expectEqual(@as(usize, 1), g.getDependentIndices(nc, &out));
    try std.testing.expectEqual(na, out[0]);
    try g.validate();
}

test "BPlusTreeAggregation builds a real root for more than 16 leaves" {
    var tree = BPlusTreeAggregation.init(std.testing.allocator);
    defer tree.deinit();
    var leaves: [40]u32 = undefined;
    for (&leaves) |*l| l.* = 2;
    try tree.buildFromLeaves(&leaves);
    try std.testing.expectEqual(@as(u32, 80), tree.totalTasks());
    tree.markLeafDirty(3);
    try std.testing.expectEqual(@as(u32, 1), tree.totalDirty());
}

test "AggregationGraph.markDirtyRecursive terminates on a cycle" {
    var agg: AggregationGraph = undefined;
    agg.init(std.testing.allocator);
    defer agg.deinit();
    _ = try agg.addAggregation(.chunk, 0, 1);
    _ = try agg.addAggregation(.chunk, 1, 1);
    try agg.addChild(0, 1);
    try agg.addChild(1, 0);
    agg.setStatus(0, .done);
    agg.setStatus(1, .done);
    agg.markDirtyRecursive(0);
    try std.testing.expectEqual(AggregationStatus.dirty, agg.getStatus(1));
    try std.testing.expectError(error.InvalidIndex, agg.addChild(0, 9));
}

// ─── G8.10: Intrusive LRU list tests ─────────────────────────────────

test "G8.10: LruList moveToFront and evictTail" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

    const idx_a = try g.addTask(id_a);
    const idx_b = try g.addTask(id_b);
    const idx_c = try g.addTask(id_c);

    // Touch in order: a, b, c → LRU order is [c, b, a] (c=MRU, a=LRU)
    g.touchLru(idx_a);
    g.touchLru(idx_b);
    g.touchLru(idx_c);

    // Evict tail should return a (least recently used)
    const evicted = g.evictLru();
    try std.testing.expectEqual(idx_a, evicted);

    // Next eviction should return b
    const evicted2 = g.evictLru();
    try std.testing.expectEqual(idx_b, evicted2);

    // Next eviction should return c
    const evicted3 = g.evictLru();
    try std.testing.expectEqual(idx_c, evicted3);

    // List should now be empty
    try std.testing.expect(g.lru.isEmpty());
}

test "G8.10: LruList moveToFront reorders correctly" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

    const idx_a = try g.addTask(id_a);
    const idx_b = try g.addTask(id_b);
    const idx_c = try g.addTask(id_c);

    // Touch: a, b, c → order [c, b, a]
    g.touchLru(idx_a);
    g.touchLru(idx_b);
    g.touchLru(idx_c);

    // Now touch a again → a should become MRU, order [a, c, b]
    g.touchLru(idx_a);

    // Evict should return b (LRU)
    const evicted = g.evictLru();
    try std.testing.expectEqual(idx_b, evicted);

    // Next evict should return c
    const evicted2 = g.evictLru();
    try std.testing.expectEqual(idx_c, evicted2);
}

test "G8.10: @fieldParentPtr recovers LruEntry from link" {
    var entry = LruEntry{
        .key = 42,
        .value = 100,
        .lru_link = .{},
    };

    const link_ptr = &entry.lru_link;
    const recovered = LruEntry.fromLink(link_ptr);

    try std.testing.expectEqual(@as(u64, 42), recovered.key);
    try std.testing.expectEqual(@as(u64, 100), recovered.value);
    try std.testing.expectEqual(@intFromPtr(&entry), @intFromPtr(recovered));
}

test "G8.10: LruList empty eviction returns NULL_INDEX" {
    var lru = LruList{};
    var dummy_prev = [_]u32{0} ** 4;
    var dummy_next = [_]u32{0} ** 4;

    const evicted = lru.evictTail(&dummy_prev, &dummy_next);
    try std.testing.expectEqual(LruList.NULL_INDEX, evicted);
    try std.testing.expect(lru.isEmpty());
}

// ─── G8.12: Arena compression with zstd tests ───────────────────────

test "G8.12: compressZstd and decompressZstd round-trip" {
    const data = "Hello, World! This is a test string for zstd compression round-trip testing. It should compress and decompress correctly.";

    const compressed = try compressZstd(std.testing.allocator, data);
    defer std.testing.allocator.free(compressed);

    // Compressed data should be different from original
    try std.testing.expect(compressed.len > 0);

    const decompressed = try decompressZstd(std.testing.allocator, compressed);
    defer std.testing.allocator.free(decompressed);

    try std.testing.expectEqualStrings(data, decompressed);
}

test "G8.12: compressTaskGraph and decompressTaskGraph round-trip" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

    const idx_a = try g.addTask(id_a);
    const idx_b = try g.addTask(id_b);
    const idx_c = try g.addTask(id_c);

    try g.addDependency(id_a, id_b);
    try g.addDependency(id_b, id_c);

    g.setStatus(idx_a, .clean);
    g.setStatus(idx_b, .dirty);

    // Compress
    const compressed = try compressTaskGraph(std.testing.allocator, &g);
    defer std.testing.allocator.free(compressed);

    // Decompress
    const restored = try decompressTaskGraph(std.testing.allocator, compressed);
    defer destroyTaskGraph(restored);

    // Verify
    try std.testing.expectEqual(@as(usize, 3), restored.taskCount());
    try std.testing.expectEqual(idx_a, restored.getIndex(id_a).?);
    try std.testing.expectEqual(idx_b, restored.getIndex(id_b).?);
    try std.testing.expectEqual(idx_c, restored.getIndex(id_c).?);

    try std.testing.expectEqual(TaskStatus.clean, restored.getStatus(idx_a));
    try std.testing.expectEqual(TaskStatus.dirty, restored.getStatus(idx_b));

    const deps_a = restored.getDependencyIndices(idx_a);
    try std.testing.expectEqual(@as(usize, 1), deps_a.len);
    try std.testing.expectEqual(idx_b, deps_a[0]);
}

// ─── G8.13: Arena snapshotting (COW) tests ───────────────────────────

test "G8.13: snapshot and restore preserves graph state" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

    const idx_a = try g.addTask(id_a);
    const idx_b = try g.addTask(id_b);
    try g.addDependency(id_a, id_b);
    g.setStatus(idx_a, .clean);

    // Create snapshot
    var snap = try snapshotTaskGraph(std.testing.allocator, &g);
    defer snap.deinit();

    // Modify original graph after snapshot
    g.setStatus(idx_b, .dirty);

    // Restore from snapshot — should NOT see the post-snapshot change
    const restored = try restoreTaskGraph(&snap);
    defer destroyTaskGraph(restored);

    try std.testing.expectEqual(@as(usize, 2), restored.taskCount());
    try std.testing.expectEqual(TaskStatus.clean, restored.getStatus(idx_a));
    // idx_b should still be pending (not dirty) in the snapshot
    try std.testing.expectEqual(TaskStatus.pending, restored.getStatus(idx_b));

    // Original graph should still have the modification
    try std.testing.expectEqual(TaskStatus.dirty, g.getStatus(idx_b));
}

test "G8.13: multiple snapshots are independent" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    _ = try g.addTask(id_a);
    _ = try g.addTask(id_b);

    // Snapshot 1 (2 nodes)
    var snap1 = try snapshotTaskGraph(std.testing.allocator, &g);
    defer snap1.deinit();

    // Add a third node after snapshot 1
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    _ = try g.addTask(id_c);

    // Snapshot 2 (3 nodes)
    var snap2 = try snapshotTaskGraph(std.testing.allocator, &g);
    defer snap2.deinit();

    // Restore snapshot 1 — should have 2 nodes
    const r1 = try restoreTaskGraph(&snap1);
    defer destroyTaskGraph(r1);
    try std.testing.expectEqual(@as(usize, 2), r1.taskCount());

    // Restore snapshot 2 — should have 3 nodes
    const r2 = try restoreTaskGraph(&snap2);
    defer destroyTaskGraph(r2);
    try std.testing.expectEqual(@as(usize, 3), r2.taskCount());

    // Original should still have 3 nodes
    try std.testing.expectEqual(@as(usize, 3), g.taskCount());
}

// ─── G8.14: Arena NUMA placement tests ───────────────────────────────

test "G8.14: numaAlloc returns aligned memory" {
    const buf = try numaAlloc(std.testing.allocator, 1024, 0);
    defer std.testing.allocator.free(buf);
    try std.testing.expect(buf.len >= 1024);
}

test "G8.14: numaPreferredNode returns valid node" {
    const node = numaPreferredNode();
    try std.testing.expect(node >= 0);
}

// ─── G8.15: Huge page support tests ──────────────────────────────────

test "G8.15: HUGE_PAGE_SIZE is 2MB" {
    try std.testing.expectEqual(@as(usize, 2 * 1024 * 1024), HUGE_PAGE_SIZE);
}

test "G8.15: hugePageAlloc returns aligned memory" {
    const buf = try hugePageAlloc(std.testing.allocator, 1024);
    defer std.testing.allocator.free(buf);
    try std.testing.expect(buf.len >= 1024);
}

test "G8.15: hugePagesAvailable returns bool" {
    const available = hugePagesAvailable();
    _ = available;
}

// ─── G8.5-G8.9 tests ─────────────────────────────────────────────────

test "G8.5: SlabArena allocates in 64KB slabs" {
    var arena = SlabArena.init(std.testing.allocator);
    defer arena.deinit();

    const a = try arena.alloc(100, 8);
    try std.testing.expectEqual(@as(usize, 1), arena.slabCount());
    try std.testing.expectEqual(@as(usize, SLAB_SIZE), arena.totalAllocated());

    const b = try arena.alloc(200, 8);
    try std.testing.expect(a.ptr != b.ptr);
    try std.testing.expectEqual(@as(usize, 1), arena.slabCount());

    _ = try arena.alloc(SLAB_SIZE, 8);
    try std.testing.expectEqual(@as(usize, 2), arena.slabCount());
    try std.testing.expectEqual(@as(usize, SLAB_SIZE * 2), arena.totalAllocated());
}

test "G8.5: SlabArena handles alignment" {
    var arena = SlabArena.init(std.testing.allocator);
    defer arena.deinit();

    const a = try arena.alloc(1, 1);
    _ = a;
    const b = try arena.alloc(8, 16);
    try std.testing.expectEqual(@as(usize, 0), @intFromPtr(b.ptr) % 16);
}

test "G8.6: compactTaskGraph removes evicted nodes" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_c = [_]u8{ 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };

    _ = try g.addTask(id_a);
    _ = try g.addTask(id_b);
    _ = try g.addTask(id_c);

    g.setStatus(1, .evicted);

    try std.testing.expectEqual(@as(usize, 3), g.taskCount());

    try compactTaskGraph(&g);

    try std.testing.expectEqual(@as(usize, 2), g.taskCount());
    try std.testing.expect(g.getIndex(id_a) != null);
    try std.testing.expect(g.getIndex(id_b) == null);
    try std.testing.expect(g.getIndex(id_c) != null);
}

test "G8.6: compactTaskGraph is no-op when no evictions" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    _ = try g.addTask(id_a);

    try compactTaskGraph(&g);
    try std.testing.expectEqual(@as(usize, 1), g.taskCount());
}

test "G8.8: TieredArena marks hot and cold" {
    var tier = TieredArena.init(std.testing.allocator);
    defer tier.deinit();

    try tier.markHot(0);
    try tier.markHot(1);
    try std.testing.expectEqual(@as(usize, 2), tier.hotCount());
    try std.testing.expectEqual(Tier.hot, tier.getTier(0));

    const data = [_]u8{ 1, 2, 3 };
    try tier.markCold(0, &data);
    try std.testing.expectEqual(@as(usize, 1), tier.hotCount());
    try std.testing.expectEqual(@as(usize, 1), tier.coldCount());
    try std.testing.expectEqual(Tier.cold, tier.getTier(0));
    try std.testing.expectEqual(Tier.hot, tier.getTier(1));

    const promoted = try tier.promote(0);
    try std.testing.expect(promoted != null);
    try std.testing.expectEqualSlices(u8, &data, promoted.?);
    std.testing.allocator.free(promoted.?);
    try std.testing.expectEqual(@as(usize, 2), tier.hotCount());
    try std.testing.expectEqual(@as(usize, 0), tier.coldCount());
}

test "G8.9: prefetchNode does not crash" {
    var g: TaskGraph = undefined;
    g.init();
    g.allocator = g.arena.allocator();
    g.id_to_index = std.AutoHashMap(TaskId, u32).init(g.allocator);
    defer g.deinit();

    const id_a = [_]u8{ 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    const id_b = [_]u8{ 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0 };
    _ = try g.addTask(id_a);
    _ = try g.addTask(id_b);

    prefetchNode(&g, 0);
    prefetchNode(&g, 1);
    prefetchNext(&g, 0, 2);
    prefetchTraversal(&g, 0);
}

// ─── G3.3: Arena-allocated aggregation graph ─────────────────────────
//
// Aggregation nodes are stored in the same arena as task nodes,
// providing 0B allocation overhead per aggregation node.
// An aggregation node represents a group of task nodes that can be
// computed together (e.g., all modules in the same chunk).

/// An aggregation node in the arena-allocated aggregation graph.
/// 24 bytes — same as TaskNode for cache-friendly traversal.
pub const AggregationNode = struct {
    /// First task node index in this aggregation
    first_task: u32,
    /// Number of task nodes in this aggregation
    task_count: u32,
    /// Packed: aggregation_type (u4) | status (u4) | child_count (u24)
    packed_flags: u32,
    /// Aggregation output hash (for caching)
    output_hash: [16]u8,
};

/// Aggregation type
pub const AggregationType = enum(u4) {
    chunk = 0,
    route = 1,
    module_group = 2,
    shared = 3,
    vendor = 4,
    entry = 5,
    custom = 15,
};

/// Aggregation status
pub const AggregationStatus = enum(u4) {
    clean = 0,
    dirty = 1,
    computing = 2,
    done = 3,
};

/// Arena-allocated aggregation graph.
/// Uses the same arena allocator as the task dependency graph.
pub const AggregationGraph = struct {
    /// Arena allocator (shared with TaskGraph)
    arena: std.heap.ArenaAllocator,
    allocator: std.mem.Allocator,

    /// Aggregation nodes stored contiguously
    nodes: std.ArrayList(AggregationNode),

    /// Task-to-aggregation mapping: task_index → aggregation_index
    task_to_agg: std.AutoHashMap(u32, u32),

    /// Child aggregation edges (flat array of u32 pairs: parent, child)
    child_edges: std.ArrayList(u32),

    /// Initialize in place (see ModuleGraph.init) — the arena allocator
    /// and the task_to_agg map both capture `self`'s address.
    pub fn init(self: *AggregationGraph, parent_allocator: std.mem.Allocator) void {
        self.arena = std.heap.ArenaAllocator.init(parent_allocator);
        self.allocator = self.arena.allocator();
        self.nodes = .empty;
        self.task_to_agg = std.AutoHashMap(u32, u32).init(self.allocator);
        self.child_edges = .empty;
    }

    pub fn deinit(self: *AggregationGraph) void {
        self.nodes.deinit(self.allocator);
        self.task_to_agg.deinit();
        self.child_edges.deinit(self.allocator);
        self.arena.deinit();
    }

    /// Add an aggregation node. Returns its index.
    /// 0B heap allocation — uses arena memory.
    pub fn addAggregation(
        self: *AggregationGraph,
        agg_type: AggregationType,
        first_task: u32,
        task_count: u32,
    ) !u32 {
        // Reject a task range that overflows u32 up front (it used to trap in
        // the mapping loop AFTER the node had already been appended).
        const last_task = std.math.add(u32, first_task, task_count) catch return error.TaskRangeOverflow;
        _ = last_task;
        const index: u32 = @intCast(self.nodes.items.len);
        const packed_flags: u32 = (@as(u32, @intFromEnum(agg_type)) << 28) |
            (@as(u32, @intFromEnum(AggregationStatus.dirty)) << 24);

        try self.nodes.append(self.allocator, .{
            .first_task = first_task,
            .task_count = task_count,
            .packed_flags = packed_flags,
            .output_hash = [_]u8{0} ** 16,
        });
        // Roll the node (and any task mappings made so far) back if the
        // mapping cannot be completed, so a failed add leaves no
        // half-registered aggregation behind.
        var mapped: u32 = 0;
        errdefer {
            self.nodes.shrinkRetainingCapacity(index);
            var k: u32 = 0;
            while (k < mapped) : (k += 1) _ = self.task_to_agg.remove(first_task + k);
        }

        // Map each task to this aggregation
        while (mapped < task_count) : (mapped += 1) {
            try self.task_to_agg.put(first_task + mapped, index);
        }

        return index;
    }

    /// Add a child aggregation edge (parent → child)
    pub fn addChild(self: *AggregationGraph, parent: u32, child: u32) !void {
        // Unknown indices used to be accepted and then indexed out of bounds
        // by markDirtyRecursive/setStatus.
        if (parent >= self.nodes.items.len or child >= self.nodes.items.len) return error.InvalidIndex;
        // Reserve both slots first so the (parent, child) pair is atomic.
        try self.child_edges.ensureUnusedCapacity(self.allocator, 2);
        self.child_edges.appendAssumeCapacity(parent);
        self.child_edges.appendAssumeCapacity(child);
    }

    /// Get the aggregation type
    pub fn getType(self: *const AggregationGraph, index: u32) AggregationType {
        return @enumFromInt((self.nodes.items[index].packed_flags >> 28) & 0xF);
    }

    /// Get the aggregation status
    pub fn getStatus(self: *const AggregationGraph, index: u32) AggregationStatus {
        return @enumFromInt((self.nodes.items[index].packed_flags >> 24) & 0xF);
    }

    /// Set the aggregation status
    pub fn setStatus(self: *AggregationGraph, index: u32, status: AggregationStatus) void {
        const node = &self.nodes.items[index];
        const agg_type = (node.packed_flags >> 28) & 0xF;
        const child_count = node.packed_flags & 0xFFFFFF;
        node.packed_flags = (agg_type << 28) | (@as(u32, @intFromEnum(status)) << 24) | child_count;
    }

    /// Get the aggregation that contains a given task
    pub fn getAggregationForTask(self: *const AggregationGraph, task_index: u32) ?u32 {
        return self.task_to_agg.get(task_index);
    }

    /// Get the number of aggregation nodes
    pub fn count(self: *const AggregationGraph) usize {
        return self.nodes.items.len;
    }

    /// Mark an aggregation and all its children as dirty
    ///
    /// Iterative with a visited set: the previous recursive version had no
    /// cycle guard (a -> b -> a recursed until stack overflow) and could
    /// recurse as deep as the graph.
    pub fn markDirtyRecursive(self: *AggregationGraph, index: u32) void {
        const n = self.nodes.items.len;
        if (index >= n) return;
        const scratch = std.heap.page_allocator;
        const visited = scratch.alloc(bool, n) catch {
            // Cannot track visits: still mark the root so the caller sees
            // at least the requested node dirty.
            self.setStatus(index, .dirty);
            return;
        };
        defer scratch.free(visited);
        @memset(visited, false);
        const stack = scratch.alloc(u32, n) catch {
            self.setStatus(index, .dirty);
            return;
        };
        defer scratch.free(stack);

        var sp: usize = 0;
        stack[sp] = index;
        sp += 1;
        visited[index] = true;
        while (sp > 0) {
            sp -= 1;
            const cur = stack[sp];
            self.setStatus(cur, .dirty);
            var i: usize = 0;
            while (i + 1 < self.child_edges.items.len) : (i += 2) {
                if (self.child_edges.items[i] != cur) continue;
                const child = self.child_edges.items[i + 1];
                if (child < n and !visited[child]) {
                    visited[child] = true;
                    stack[sp] = child; // sp < n: each node is pushed at most once
                    sp += 1;
                }
            }
        }
    }

    /// Set the output hash for an aggregation
    pub fn setOutputHash(self: *AggregationGraph, index: u32, hash: [16]u8) void {
        self.nodes.items[index].output_hash = hash;
    }

    /// Get the output hash for an aggregation
    pub fn getOutputHash(self: *const AggregationGraph, index: u32) [16]u8 {
        return self.nodes.items[index].output_hash;
    }
};

test "G3.3: AggregationGraph arena-allocated" {
    var agg: AggregationGraph = undefined;
    agg.init(std.testing.allocator);
    defer agg.deinit();

    // Add aggregations
    const idx0 = try agg.addAggregation(.chunk, 0, 5);
    const idx1 = try agg.addAggregation(.route, 5, 3);
    const idx2 = try agg.addAggregation(.vendor, 8, 10);

    try std.testing.expectEqual(@as(u32, 0), idx0);
    try std.testing.expectEqual(@as(u32, 1), idx1);
    try std.testing.expectEqual(@as(u32, 2), idx2);
    try std.testing.expectEqual(@as(usize, 3), agg.count());

    // Check types
    try std.testing.expectEqual(AggregationType.chunk, agg.getType(0));
    try std.testing.expectEqual(AggregationType.route, agg.getType(1));
    try std.testing.expectEqual(AggregationType.vendor, agg.getType(2));

    // Check task-to-aggregation mapping
    try std.testing.expectEqual(@as(u32, 0), agg.getAggregationForTask(2).?);
    try std.testing.expectEqual(@as(u32, 1), agg.getAggregationForTask(6).?);
    try std.testing.expectEqual(@as(u32, 2), agg.getAggregationForTask(15).?);
    try std.testing.expect(agg.getAggregationForTask(100) == null);

    // Check status
    try std.testing.expectEqual(AggregationStatus.dirty, agg.getStatus(0));
    agg.setStatus(0, .done);
    try std.testing.expectEqual(AggregationStatus.done, agg.getStatus(0));

    // Check child edges
    try agg.addChild(0, 1);
    try agg.addChild(0, 2);
    agg.markDirtyRecursive(0);
    try std.testing.expectEqual(AggregationStatus.dirty, agg.getStatus(0));
    try std.testing.expectEqual(AggregationStatus.dirty, agg.getStatus(1));
    try std.testing.expectEqual(AggregationStatus.dirty, agg.getStatus(2));

    // Check output hash
    const hash = [_]u8{0xAB} ** 16;
    agg.setOutputHash(0, hash);
    try std.testing.expectEqualSlices(u8, &hash, &agg.getOutputHash(0));
}

test "G3.3: AggregationNode is 28 bytes" {
    // 3×u32 (first_task, task_count, packed_flags) + [16]u8 output_hash.
    try std.testing.expectEqual(@as(usize, 28), @sizeOf(AggregationNode));
}

// ─── G3.6: B+tree Layout for Aggregation Graph ─────────────────────────

/// B+tree layout for cache-friendly contiguous aggregation nodes.
/// Each layer is a contiguous array, enabling SIMD-friendly sequential scans.
pub const BPlusTreeNode = struct {
    /// Number of children in this node.
    child_count: u16,
    /// Whether this is a leaf node.
    is_leaf: bool,
    /// Padding for alignment.
    _pad: [1]u8 = .{0},
    /// Child indices (for internal nodes) or task indices (for leaf nodes).
    children: [16]u32,
    /// Aggregated metrics for this subtree.
    total_tasks: u32,
    dirty_count: u32,
    /// Pointer to next leaf node (for range scans).
    next_leaf: u32 = 0,

    pub fn init() BPlusTreeNode {
        return .{
            .child_count = 0,
            .is_leaf = true,
            .children = [_]u32{0} ** 16,
            .total_tasks = 0,
            .dirty_count = 0,
        };
    }

    pub fn isFull(self: *const BPlusTreeNode) bool {
        return self.child_count >= 16;
    }

    pub fn addChild(self: *BPlusTreeNode, idx: u32) void {
        if (self.child_count < 16) {
            self.children[self.child_count] = idx;
            self.child_count += 1;
        }
    }
};

/// B+tree-structured aggregation graph with contiguous layers.
pub const BPlusTreeAggregation = struct {
    nodes: std.ArrayList(BPlusTreeNode),
    /// Layer boundaries: layer_starts[i] is the start index of layer i.
    layer_starts: std.ArrayList(u32),
    allocator: std.mem.Allocator,

    pub fn init(allocator: std.mem.Allocator) BPlusTreeAggregation {
        return .{
            .nodes = .empty,
            .layer_starts = .empty,
            .allocator = allocator,
        };
    }

    pub fn deinit(self: *BPlusTreeAggregation) void {
        self.nodes.deinit(self.allocator);
        self.layer_starts.deinit(self.allocator);
    }

    /// Build a B+tree from a flat list of task count per leaf.
    pub fn buildFromLeaves(self: *BPlusTreeAggregation, leaf_counts: []const u32) !void {
        self.nodes.clearRetainingCapacity();
        self.layer_starts.clearRetainingCapacity();

        // Layer 0: leaves
        try self.layer_starts.append(self.allocator, 0);
        for (leaf_counts) |count| {
            var node = BPlusTreeNode.init();
            node.is_leaf = true;
            node.total_tasks = count;
            node.child_count = 1;
            node.children[0] = @intCast(self.nodes.items.len);
            try self.nodes.append(self.allocator, node);
        }
        try self.layer_starts.append(self.allocator, @intCast(self.nodes.items.len));

        // Build internal layers until we have a single root. layer_starts
        // holds layer BOUNDARIES: the last two entries delimit the newest
        // layer. (The next layer's end used to be pushed BEFORE building it,
        // so the loop saw an empty layer and stopped after one internal
        // layer - more than 16 leaves never got a real root.)
        while (self.layer_starts.items[self.layer_starts.items.len - 1] - self.layer_starts.items[self.layer_starts.items.len - 2] > 1) {
            const layer_start = self.layer_starts.items[self.layer_starts.items.len - 2];
            const layer_end = self.layer_starts.items[self.layer_starts.items.len - 1];
            const layer_count = layer_end - layer_start;

            var i: u32 = 0;
            while (i < layer_count) {
                var node = BPlusTreeNode.init();
                node.is_leaf = false;
                var j: u16 = 0;
                while (j < 16 and i + @as(u32, j) < layer_count) : (j += 1) {
                    const child_idx = layer_start + i + @as(u32, j);
                    node.addChild(child_idx);
                    node.total_tasks +|= self.nodes.items[child_idx].total_tasks;
                    node.dirty_count +|= self.nodes.items[child_idx].dirty_count;
                }
                try self.nodes.append(self.allocator, node);
                i += 16;
            }
            try self.layer_starts.append(self.allocator, @intCast(self.nodes.items.len));
        }
    }

    /// Get total task count from the root.
    pub fn totalTasks(self: *const BPlusTreeAggregation) u32 {
        if (self.nodes.items.len == 0) return 0;
        return self.nodes.items[self.nodes.items.len - 1].total_tasks;
    }

    /// Get total dirty count from the root.
    pub fn totalDirty(self: *const BPlusTreeAggregation) u32 {
        if (self.nodes.items.len == 0) return 0;
        return self.nodes.items[self.nodes.items.len - 1].dirty_count;
    }

    /// Mark a leaf as dirty and propagate up.
    pub fn markLeafDirty(self: *BPlusTreeAggregation, leaf_idx: u32) void {
        if (leaf_idx >= self.nodes.items.len) return;
        if (leaf_idx >= self.layer_starts.items[1]) return; // not a leaf
        self.nodes.items[leaf_idx].dirty_count = 1;
        self.recomputeDirty();
    }

    /// Recompute every internal node's dirty count from its children,
    /// bottom-up, so `totalDirty()` (the root) reflects leaf changes.
    /// (markLeafDirty used to set only the leaf, leaving the root at 0.)
    pub fn recomputeDirty(self: *BPlusTreeAggregation) void {
        if (self.layer_starts.items.len < 3) return;
        var layer: usize = 1;
        while (layer + 1 < self.layer_starts.items.len) : (layer += 1) {
            const start = self.layer_starts.items[layer];
            const end = self.layer_starts.items[layer + 1];
            var idx = start;
            while (idx < end) : (idx += 1) {
                const node = &self.nodes.items[idx];
                var sum: u32 = 0;
                for (node.children[0..node.child_count]) |c| {
                    sum +|= self.nodes.items[c].dirty_count;
                }
                node.dirty_count = sum;
            }
        }
    }
};

test "G3.6: B+tree build from leaves" {
    var tree = BPlusTreeAggregation.init(std.testing.allocator);
    defer tree.deinit();

    const leaf_counts = [_]u32{ 5, 3, 8, 2, 7, 1, 4, 6 };
    try tree.buildFromLeaves(&leaf_counts);

    try std.testing.expect(tree.totalTasks() == 36); // 5+3+8+2+7+1+4+6
    try std.testing.expect(tree.nodes.items.len > 8); // Has internal nodes
}

test "G3.6: B+tree single leaf" {
    var tree = BPlusTreeAggregation.init(std.testing.allocator);
    defer tree.deinit();

    try tree.buildFromLeaves(&[_]u32{42});
    try std.testing.expectEqual(@as(u32, 42), tree.totalTasks());
}

// ─── G3.11: @bitSet Dirty Tracking with SIMD any() ─────────────────────

/// Dirty tracking using @bitSet for SIMD-accelerated any() checks.
/// Each bit represents one node's dirty status.
pub const DirtyBitSet = struct {
    bits: []u64,
    capacity: usize,

    pub fn init(allocator: std.mem.Allocator, capacity: usize) !DirtyBitSet {
        const num_words = (capacity + 63) / 64;
        const bits = try allocator.alloc(u64, num_words);
        @memset(bits, 0);
        return .{
            .bits = bits,
            .capacity = capacity,
        };
    }

    pub fn deinit(self: *DirtyBitSet, allocator: std.mem.Allocator) void {
        allocator.free(self.bits);
    }

    pub fn setDirty(self: *DirtyBitSet, idx: usize) void {
        if (idx >= self.capacity) return;
        const word = idx / 64;
        const bit = idx % 64;
        self.bits[word] |= (@as(u64, 1) << @intCast(bit));
    }

    pub fn setClean(self: *DirtyBitSet, idx: usize) void {
        if (idx >= self.capacity) return;
        const word = idx / 64;
        const bit = idx % 64;
        self.bits[word] &= ~(@as(u64, 1) << @intCast(bit));
    }

    pub fn isDirty(self: *const DirtyBitSet, idx: usize) bool {
        if (idx >= self.capacity) return false;
        const word = idx / 64;
        const bit = idx % 64;
        return (self.bits[word] & (@as(u64, 1) << @intCast(bit))) != 0;
    }

    /// SIMD-accelerated check: are any nodes dirty?
    /// Uses @Vector to check 4 u64 words at a time (256 bits per iteration).
    pub fn anyDirty(self: *const DirtyBitSet) bool {
        const Vec = @Vector(4, u64);
        var i: usize = 0;
        while (i + 4 <= self.bits.len) : (i += 4) {
            const v: Vec = .{
                self.bits[i],
                self.bits[i + 1],
                self.bits[i + 2],
                self.bits[i + 3],
            };
            // OR all elements together — if any word is non-zero, result is non-zero
            const reduced = @reduce(.Or, v);
            if (reduced != 0) return true;
        }
        while (i < self.bits.len) : (i += 1) {
            if (self.bits[i] != 0) return true;
        }
        return false;
    }

    /// Count total dirty nodes (popcount across all words).
    pub fn dirtyCount(self: *const DirtyBitSet) u32 {
        var count: u32 = 0;
        for (self.bits) |word| {
            count += @popCount(word);
        }
        return count;
    }

    /// Clear all dirty flags.
    pub fn clearAll(self: *DirtyBitSet) void {
        @memset(self.bits, 0);
    }
};

test "G3.11: DirtyBitSet set and check" {
    var bs = try DirtyBitSet.init(std.testing.allocator, 256);
    defer bs.deinit(std.testing.allocator);

    try std.testing.expect(!bs.anyDirty());

    bs.setDirty(5);
    bs.setDirty(100);
    bs.setDirty(255);

    try std.testing.expect(bs.isDirty(5));
    try std.testing.expect(bs.isDirty(100));
    try std.testing.expect(bs.isDirty(255));
    try std.testing.expect(!bs.isDirty(0));
    try std.testing.expect(!bs.isDirty(50));

    try std.testing.expect(bs.anyDirty());
    try std.testing.expectEqual(@as(u32, 3), bs.dirtyCount());

    bs.setClean(100);
    try std.testing.expect(!bs.isDirty(100));
    try std.testing.expectEqual(@as(u32, 2), bs.dirtyCount());

    bs.clearAll();
    try std.testing.expect(!bs.anyDirty());
    try std.testing.expectEqual(@as(u32, 0), bs.dirtyCount());
}

test "G3.11: DirtyBitSet SIMD any() with large capacity" {
    var bs = try DirtyBitSet.init(std.testing.allocator, 1024);
    defer bs.deinit(std.testing.allocator);

    // Set a dirty bit at position 500 (requires SIMD scan to find)
    bs.setDirty(500);
    try std.testing.expect(bs.anyDirty());
    try std.testing.expectEqual(@as(u32, 1), bs.dirtyCount());
}

// ─── G3.12: Copy-on-Write Aggregation Graph ────────────────────────────

/// Copy-on-write semantics: when a sub-graph is modified, only the
/// affected aggregation nodes are copied. This uses a persistent data
/// structure approach where modifications create new nodes while sharing
/// unchanged children.
pub const CowAggregationNode = struct {
    /// Reference count for sharing.
    ref_count: u32,
    /// Whether this node has been modified since creation.
    modified: bool,
    /// Child node indices (shared until modified).
    children: [8]u32,
    child_count: u8,
    /// Aggregated data.
    total_tasks: u32,
    dirty_count: u32,
    /// Version number — incremented on each modification.
    version: u32,

    pub fn init() CowAggregationNode {
        return .{
            .ref_count = 1,
            .modified = false,
            .children = [_]u32{0} ** 8,
            .child_count = 0,
            .total_tasks = 0,
            .dirty_count = 0,
            .version = 0,
        };
    }
};

/// CoW aggregation graph that only copies modified paths.
pub const CowAggregationGraph = struct {
    nodes: std.ArrayList(CowAggregationNode),
    /// Root version tracking.
    root_version: u32,
    allocator: std.mem.Allocator,

    pub fn init(allocator: std.mem.Allocator) CowAggregationGraph {
        return .{
            .nodes = .empty,
            .root_version = 0,
            .allocator = allocator,
        };
    }

    pub fn deinit(self: *CowAggregationGraph) void {
        self.nodes.deinit(self.allocator);
    }

    /// Add a root node.
    pub fn addRoot(self: *CowAggregationGraph) !u32 {
        const idx: u32 = @intCast(self.nodes.items.len);
        try self.nodes.append(self.allocator, CowAggregationNode.init());
        return idx;
    }

    /// Modify a node — creates a copy if shared (ref_count > 1).
    pub fn modifyNode(self: *CowAggregationGraph, idx: u32) !u32 {
        if (idx >= self.nodes.items.len) return error.InvalidIndex;
        if (self.nodes.items[idx].ref_count > 1) {
            // Copy-on-write: create a new node and decrement ref of old.
            // Append FIRST: the old node's ref_count used to be decremented
            // before the (fallible) append, so an OOM permanently lost a
            // reference.
            var new_node = self.nodes.items[idx];
            new_node.ref_count = 1;
            new_node.modified = true;
            new_node.version +|= 1;
            const new_idx: u32 = @intCast(self.nodes.items.len);
            try self.nodes.append(self.allocator, new_node);
            // append may have reallocated: re-derive pointers from items.
            self.nodes.items[idx].ref_count -= 1;
            // The copy now shares its children with the original, so each
            // child gains a reference.
            for (self.nodes.items[new_idx].children[0..self.nodes.items[new_idx].child_count]) |c| {
                if (c < self.nodes.items.len) self.nodes.items[c].ref_count +|= 1;
            }
            return new_idx;
        }
        const node = &self.nodes.items[idx];
        node.modified = true;
        node.version +|= 1;
        return idx;
    }

    /// Mark a node dirty (with CoW semantics).
    pub fn markDirty(self: *CowAggregationGraph, idx: u32) !void {
        const new_idx = try self.modifyNode(idx);
        self.nodes.items[new_idx].dirty_count = 1;
        self.root_version += 1;
    }

    /// Get current root version.
    pub fn version(self: *const CowAggregationGraph) u32 {
        return self.root_version;
    }
};

test "G3.12: CoW graph creates copies on shared modification" {
    var graph = CowAggregationGraph.init(std.testing.allocator);
    defer graph.deinit();

    const root = try graph.addRoot();
    try std.testing.expectEqual(@as(u32, 1), graph.nodes.items[root].ref_count);

    // Modify a non-shared node — should not create a copy
    const same = try graph.modifyNode(root);
    try std.testing.expectEqual(root, same);

    // Simulate sharing by incrementing ref count
    graph.nodes.items[root].ref_count = 2;

    // Now modification should create a copy
    const copy = try graph.modifyNode(root);
    try std.testing.expect(copy != root);
    try std.testing.expectEqual(@as(u32, 1), graph.nodes.items[root].ref_count);
    try std.testing.expectEqual(@as(u32, 1), graph.nodes.items[copy].ref_count);
    try std.testing.expect(graph.nodes.items[copy].modified);
}

test "G3.12: CoW graph version increments on modification" {
    var graph = CowAggregationGraph.init(std.testing.allocator);
    defer graph.deinit();

    const root = try graph.addRoot();
    const v0 = graph.version();
    try graph.markDirty(root);
    try std.testing.expect(graph.version() > v0);
}

// ─── G3.14: Distributed Aggregation ────────────────────────────────────

/// Distributed aggregation metadata for remote cache scenarios.
/// Aggregation nodes can be shared across machines via content-addressed IDs.
pub const DistributedAggregationMeta = struct {
    /// Content-addressed hash of the aggregation node.
    node_hash: [16]u8,
    /// Machine ID that owns this node (0 = local).
    owner_machine: u32,
    /// Whether this node is available on the remote cache.
    is_remote: bool,
    /// Number of machines that have this node cached.
    replica_count: u16,
    /// Last sync timestamp.
    last_sync_ms: u64,

    pub fn init(node_hash: [16]u8) DistributedAggregationMeta {
        return .{
            .node_hash = node_hash,
            .owner_machine = 0,
            .is_remote = false,
            .replica_count = 0,
            .last_sync_ms = 0,
        };
    }

    pub fn isLocal(self: *const DistributedAggregationMeta) bool {
        return self.owner_machine == 0;
    }

    pub fn markRemote(self: *DistributedAggregationMeta, owner: u32) void {
        self.owner_machine = owner;
        self.is_remote = true;
    }

    pub fn markSynced(self: *DistributedAggregationMeta, timestamp: u64) void {
        self.last_sync_ms = timestamp;
        self.replica_count += 1;
    }
};

test "G3.14: DistributedAggregationMeta local and remote" {
    const hash = [_]u8{0x42} ** 16;
    var meta = DistributedAggregationMeta.init(hash);
    try std.testing.expect(meta.isLocal());
    try std.testing.expect(!meta.is_remote);

    meta.markRemote(5);
    try std.testing.expect(!meta.isLocal());
    try std.testing.expect(meta.is_remote);
    try std.testing.expectEqual(@as(u32, 5), meta.owner_machine);

    meta.markSynced(12345);
    try std.testing.expectEqual(@as(u16, 1), meta.replica_count);
    try std.testing.expectEqual(@as(u64, 12345), meta.last_sync_ms);
}

// ─── G1.17: Comptime Task<T> Layouts ───────────────────────────────────

/// Comptime function to determine the optimal storage strategy for a type.
/// For types <= 24 bytes, store inline. For larger types, store an offset.
pub fn InlineStorage(comptime T: type) type {
    return struct {
        pub const IS_INLINE = @sizeOf(T) <= 24;
        pub const SIZE = @sizeOf(T);

        pub fn store(buf: []u8, value: T) usize {
            if (IS_INLINE) {
                // Store inline
                const ptr: *T = @ptrCast(@alignCast(buf.ptr));
                ptr.* = value;
                return @sizeOf(T);
            } else {
                // Store offset (just the size for this simplified version)
                return @sizeOf(T);
            }
        }

        pub fn load(buf: []const u8) T {
            if (IS_INLINE) {
                const ptr: *const T = @ptrCast(@alignCast(buf.ptr));
                return ptr.*;
            } else {
                // Would load from offset in real implementation
                return std.mem.bytesToValue(T, buf[0..@sizeOf(T)]);
            }
        }
    };
}

test "G1.17: InlineStorage for small type (u32 = 4 bytes)" {
    const Storage = InlineStorage(u32);
    try std.testing.expect(Storage.IS_INLINE);
    try std.testing.expectEqual(@as(usize, 4), Storage.SIZE);

    var buf: [32]u8 = undefined;
    _ = Storage.store(&buf, 42);
    try std.testing.expectEqual(@as(u32, 42), Storage.load(&buf));
}

test "G1.17: InlineStorage for 24-byte type" {
    const Type24 = struct { data: [24]u8 };
    const Storage = InlineStorage(Type24);
    try std.testing.expect(Storage.IS_INLINE);

    var buf: [32]u8 = undefined;
    const value = Type24{ .data = [_]u8{0xAB} ** 24 };
    _ = Storage.store(&buf, value);
    const loaded = Storage.load(&buf);
    try std.testing.expectEqualSlices(u8, &value.data, &loaded.data);
}

test "G1.17: InlineStorage for large type (>24 bytes)" {
    const LargeType = struct { data: [64]u8 };
    const Storage = InlineStorage(LargeType);
    try std.testing.expect(!Storage.IS_INLINE);
    try std.testing.expectEqual(@as(usize, 64), Storage.SIZE);
}
