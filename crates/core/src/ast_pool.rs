//! AST Pool — Parse-once shared AST cache.
//!
//! Eliminates redundant Oxc parses by caching the pre-transform `Program` in an
//! in-memory pool. Multiple consumers (transform, dynamic import detection, i18n
//! extraction) read from the same parsed AST instead of re-parsing the source.
//!
//! # Design
//!
//! The pool stores `Box<AstEntry>` keyed by a content hash of the source.
//! Each `AstEntry` owns an `Allocator` and a `Program` that references into it.
//! The `Allocator` is heap-allocated via `Box`, so its arena address is stable
//! even when the `AstEntry` is moved (e.g., during `HashMap` rehash — the `Box`
//! pointer moves, but the heap data doesn't).
//!
//! The `Program<'a>` lifetime is erased to `'static` via `unsafe` transmute.
//! This is safe because:
//! 1. The `Allocator` is stored in the same `Box` and never dropped while the
//!    `Program` is alive.
//! 2. The `Allocator`'s arena memory is stable (bumpalo-style — allocations
//!    are never moved or freed until the allocator is dropped).
//! 3. Access is mediated through the pool API, which ties the borrow to the
//!    pool's lifetime.

use std::collections::HashMap;

use oxc::allocator::Allocator;
use oxc::ast::ast::Program;
use oxc::parser::{Parser, ParserReturn};
use oxc::span::SourceType;

/// A handle to a parsed AST in the pool.
/// The handle is the content hash of the source — same source always produces
/// the same handle.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq, Ord, PartialOrd)]
pub struct AstHandle(pub u64);

impl AstHandle {
    /// Compute a handle from source code.
    /// Uses FNV-1a (fast, non-cryptographic) — the handle is only used as an
    /// in-memory cache key, not for content-addressed storage.
    pub fn from_source(source: &str) -> Self {
        // FNV-1a 64-bit
        let mut hash: u64 = 0xcbf29ce484222325;
        for &b in source.as_bytes() {
            hash ^= b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        AstHandle(hash)
    }
}

/// A pre-parsed AST that has been removed from the pool and is ready for
/// transformation. This is a self-contained `(Allocator, Program)` pair that
/// can be moved across threads (the `Allocator` is `Box`ed, so its heap
/// address is stable).
///
/// Created by `AstPool::pre_parse()` — used for the parallel rayon transform
/// path where each worker needs its own AST without shared mutable state.
pub struct PreParsedAst {
    allocator: Box<Allocator>,
    /// Program with lifetime erased to 'static.
    program: Program<'static>,
}

// SAFETY: Same justification as AstEntry — the allocator is Boxed (stable
// heap address), the program references the allocator's arena.
unsafe impl Send for PreParsedAst {}
unsafe impl Sync for PreParsedAst {}

impl PreParsedAst {
    /// Get a reference to the parsed program.
    /// The lifetime is tied to `&self` — the program references the allocator
    /// which is owned by this struct.
    ///
    /// # Safety
    ///
    /// The returned `Program<'_>` borrows from the `PreParsedAst`'s `Allocator`.
    /// The caller must ensure the `PreParsedAst` (and its allocator) outlives all
    /// uses of the returned `Program`. Dropping the `PreParsedAst` while the
    /// `Program` is still in use causes undefined behavior because the
    /// arena memory the `Program` references would be freed.
    pub fn program(&self) -> &Program<'_> {
        // SAFETY: The program references self.allocator, which is alive
        // as long as self is alive.
        unsafe { std::mem::transmute(&self.program) }
    }

    /// Get a mutable reference to the parsed program.
    ///
    /// # Safety
    ///
    /// The returned `&mut Program<'_>` borrows from the `PreParsedAst`'s
    /// `Allocator`. The caller must ensure the `PreParsedAst` (and its
    /// allocator) outlives all uses of the returned `Program`. Mutating the
    /// program must not introduce references to memory outside the
    /// allocator's arena.
    pub fn program_mut(&mut self) -> &mut Program<'_> {
        // SAFETY: The program references self.allocator, which is alive
        // as long as self is alive.
        unsafe { std::mem::transmute(&mut self.program) }
    }

    /// Get a reference to the allocator.
    pub fn allocator(&self) -> &Allocator {
        &self.allocator
    }

    /// Decompose into owned allocator and program.
    /// The caller must keep them alive together.
    ///
    /// # Safety
    ///
    /// The returned `Program<'static>` references memory inside the returned
    /// `Box<Allocator>`'s arena. The caller is responsible for keeping the
    /// `Allocator` alive for as long as the `Program` is in use. Dropping the
    /// `Allocator` while the `Program` is still referenced causes undefined
    /// behavior. The `'static` lifetime is a lie (erased via `transmute`) and
    /// must not be used to escape the allocator's lifetime.
    pub fn into_parts(self) -> (Box<Allocator>, Program<'static>) {
        (self.allocator, self.program)
    }
}

/// An owned AST entry — allocator + program pair.
/// The program references memory inside the allocator's arena.
struct AstEntry {
    allocator: Box<Allocator>,
    /// Program with lifetime erased to 'static.
    /// Safe because the allocator is owned by this struct and never moved
    /// (it's behind a Box — the heap address is stable).
    program: Program<'static>,
    /// Parser diagnostics from the initial parse.
    panicked: bool,
}

// SAFETY: AstEntry owns both the Allocator and the Program.
// The Program references the Allocator's arena, which is stable heap memory.
// Access is single-threaded (the pool uses &mut self for all operations).
unsafe impl Send for AstEntry {}
unsafe impl Sync for AstEntry {}

/// An AST pool that caches parsed Oxc `Program` values.
///
/// The pool eliminates redundant parses by storing the pre-transform AST.
/// Consumers can:
/// - `get_or_parse()` — parse source if not cached, return a handle
/// - `with_program()` — run a closure with read-only access to the AST
/// - `take()` — remove the entry and take ownership (for transformation)
///
/// # Example
///
/// ```no_run
/// use pledgepack_core::ast_pool::AstPool;
/// use oxc::span::SourceType;
///
/// let mut pool = AstPool::new();
/// let handle = pool.get_or_parse("const x = 1;", SourceType::mjs()).unwrap();
///
/// // Read-only access for multiple consumers
/// pool.with_program(handle, |program| {
///     // Visit AST for dynamic import detection, i18n extraction, etc.
///     let _stmts = &program.body;
/// });
///
/// // Take ownership for transformation
/// let (allocator, mut program) = pool.take(handle).unwrap();
/// // ... transform program ...
/// ```
pub struct AstPool {
    entries: HashMap<u64, Box<AstEntry>>,
    /// Maximum number of entries before LRU eviction (0 = unlimited).
    max_entries: usize,
    /// Track insertion order for LRU eviction (simple approach).
    insertion_order: Vec<u64>,
}

impl AstPool {
    /// Create a new AST pool with unlimited capacity.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: 0,
            insertion_order: Vec::new(),
        }
    }

    /// Create a new AST pool with a maximum number of entries.
    /// When the limit is reached, the oldest entry is evicted (FIFO).
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
            insertion_order: Vec::new(),
        }
    }

    /// Parse source code and cache the AST, or return a handle to the cached AST.
    ///
    /// Returns an error if parsing fails (panicked).
    /// The handle can be used with `with_program()` or `take()`.
    pub fn get_or_parse(
        &mut self,
        source: &str,
        source_type: SourceType,
    ) -> Result<AstHandle, String> {
        let handle = AstHandle::from_source(source);

        // Cache hit — return existing handle
        if self.entries.contains_key(&handle.0) {
            return Ok(handle);
        }

        // Cache miss — parse and store
        self.parse_and_store(handle, source, source_type)?;
        Ok(handle)
    }

    /// Parse source code and return a `PreParsedAst` that is NOT stored in the pool.
    ///
    /// This is for the parallel rayon transform path: pre-parse all modules
    /// sequentially, then move the `PreParsedAst` values into parallel workers.
    /// Each worker gets its own self-contained `(Allocator, Program)` pair —
    /// no shared mutable state needed.
    ///
    /// If the source is already in the pool, it is removed and returned.
    /// If not, it is parsed fresh (and not stored in the pool).
    pub fn pre_parse(
        &mut self,
        source: &str,
        source_type: SourceType,
    ) -> Result<PreParsedAst, String> {
        let handle = AstHandle::from_source(source);

        // If already in pool, take it
        if let Some((allocator, program)) = self.take(handle) {
            return Ok(PreParsedAst {
                allocator,
                program,
            });
        }

        // Parse fresh (not stored in pool)
        let allocator = Box::new(Allocator::default());
        let ParserReturn {
            program,
            panicked,
            ..
        } = Parser::new(&allocator, source, source_type).parse();

        if panicked {
            return Err("Parser panicked".to_string());
        }

        let program_static: Program<'static> = unsafe { std::mem::transmute(program) };

        Ok(PreParsedAst {
            allocator,
            program: program_static,
        })
    }

    /// Parse source code, always storing a fresh entry (even if cached).
    /// Useful when the source_type may differ from a cached entry.
    pub fn parse_and_store(
        &mut self,
        handle: AstHandle,
        source: &str,
        source_type: SourceType,
    ) -> Result<(), String> {
        let allocator = Box::new(Allocator::default());
        let ParserReturn {
            program,
            panicked,
            ..
        } = Parser::new(&allocator, source, source_type).parse();

        if panicked {
            return Err("Parser panicked".to_string());
        }

        // SAFETY: The allocator is stored in a Box (stable heap address).
        // The program references the allocator's arena, which doesn't move.
        // Both are stored in AstEntry, so the allocator outlives the program.
        let program_static: Program<'static> = unsafe { std::mem::transmute(program) };

        // Evict if at capacity
        if self.max_entries > 0 && self.entries.len() >= self.max_entries {
            if let Some(&oldest) = self.insertion_order.first() {
                self.entries.remove(&oldest);
                self.insertion_order.remove(0);
            }
        }

        self.entries.insert(
            handle.0,
            Box::new(AstEntry {
                allocator,
                program: program_static,
                panicked,
            }),
        );
        self.insertion_order.push(handle.0);

        Ok(())
    }

    /// Run a closure with read-only access to the parsed `Program`.
    ///
    /// The closure receives `&Program<'_>` — the lifetime is tied to the
    /// borrow of the pool entry, not the pool itself (due to the 'static
    /// transmute, we need to constrain the lifetime here).
    pub fn with_program<R, F>(&self, handle: AstHandle, f: F) -> Option<R>
    where
        F: FnOnce(&Program<'_>) -> R,
    {
        let entry = self.entries.get(&handle.0)?;
        // SAFETY: The program references the entry's allocator, which is alive
        // as long as the entry is in the pool. We constrain the lifetime to
        // the borrow of the entry.
        let program: &Program<'_> = unsafe { std::mem::transmute(&entry.program) };
        Some(f(program))
    }

    /// Run a closure with mutable access to the parsed `Program`.
    ///
    /// This is for cases where the AST needs to be mutated in-place before
    /// transformation (e.g., plugin transforms).
    pub fn with_program_mut<R, F>(&mut self, handle: AstHandle, f: F) -> Option<R>
    where
        F: FnOnce(&mut Program<'_>) -> R,
    {
        let entry = self.entries.get_mut(&handle.0)?;
        let program: &mut Program<'_> = unsafe { std::mem::transmute(&mut entry.program) };
        Some(f(program))
    }

    /// Take ownership of the AST entry, removing it from the pool.
    ///
    /// This is used when the program needs to be transformed (mutated) and
    /// codegen'd. After transformation, the program is consumed and can't be
    /// shared anymore.
    ///
    /// Returns `(Allocator, Program)` — the caller owns both and must keep
    /// them alive together.
    ///
    /// # Safety
    ///
    /// The returned `Program<'static>` references memory inside the returned
    /// `Box<Allocator>`'s arena. The `'static` lifetime is erased via
    /// `transmute` and is not real — the caller must keep the `Allocator`
    /// alive for as long as the `Program` is in use. Dropping the `Allocator`
    /// while the `Program` is still referenced causes undefined behavior
    /// (use-after-free of arena memory).
    pub fn take(&mut self, handle: AstHandle) -> Option<(Box<Allocator>, Program<'static>)> {
        let entry = self.entries.remove(&handle.0)?;
        self.insertion_order.retain(|&h| h != handle.0);

        // Destructure the Box<AstEntry>
        let AstEntry {
            allocator,
            program,
            ..
        } = *entry;

        Some((allocator, program))
    }

    /// Check if a handle is in the pool.
    pub fn contains(&self, handle: AstHandle) -> bool {
        self.entries.contains_key(&handle.0)
    }

    /// Number of cached ASTs in the pool.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clear all cached ASTs.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
    }

    /// Get the parser diagnostics for a cached entry.
    pub fn panicked(&self, handle: AstHandle) -> Option<bool> {
        self.entries.get(&handle.0).map(|e| e.panicked)
    }
}

impl Default for AstPool {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Plugin AST Access Trait ─────────────────────────────────────────
//
// Phase 4: Define the interface that plugins will use to access the AST.
//
// The full WASM plugin ABI (Phase 0: WIT contract, Phase 2: Wasmtime host)
// is not yet implemented. The current plugin system uses QuickJS (JS engine)
// via `js-plugin-host`. This trait defines the interface that:
//
// 1. The current QuickJS-based plugin host can implement (via JSON serialization)
// 2. The future WASM-based plugin host will implement (via WIT + Wasmtime)
//
// The key insight: plugins don't need the raw arena-allocated AST. They need
// a *serializable* representation (ESTree JSON is the de facto standard).
// The trait abstracts over the serialization format.

/// A serializable AST representation for plugin access.
///
/// This is the format that plugins receive when they request AST access.
/// The default implementation uses ESTree-compatible JSON (the format that
/// Babel, ESLint, and most JS tooling expects).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginAst {
    /// The source code that was parsed
    pub source: String,
    /// The AST as ESTree-compatible JSON
    pub ast_json: String,
    /// The module kind (JS, TS, TSX, JSX)
    pub kind: String,
    /// The file path
    pub file_path: String,
}

/// Trait for providing AST access to plugins.
///
/// Implementors:
/// - `AstPool` (in-process, direct access)
/// - Future: WASM plugin host (via WIT + Wasmtime, with serialization)
///
/// This trait is the boundary between the arena-allocated Oxc AST (which is
/// not serializable) and the plugin-visible representation (which must be
/// serializable for WASM sandboxing).
pub trait PluginAstSource {
    /// Get a serializable AST for the given source.
    ///
    /// If the source is already in the pool, the AST is serialized from the
    /// cached parse. If not, it is parsed on demand.
    fn get_plugin_ast(&mut self, source: &str, file_path: &str, kind: &str) -> Result<PluginAst, String>;
}

impl PluginAstSource for AstPool {
    fn get_plugin_ast(&mut self, source: &str, file_path: &str, kind: &str) -> Result<PluginAst, String> {
        let path = std::path::Path::new(file_path);
        let source_type = oxc::span::SourceType::from_path(path).unwrap_or_else(|_| {
            match kind {
                "tsx" => oxc::span::SourceType::tsx(),
                "ts" | "typescript" => oxc::span::SourceType::ts(),
                "jsx" => oxc::span::SourceType::jsx(),
                _ => oxc::span::SourceType::mjs(),
            }
        });

        let handle = self.get_or_parse(source, source_type)?;

        // Serialize the AST to ESTree-compatible JSON via the hand-written
        // Oxc → ESTree converter. This gives WASM and JS plugins full AST
        // access (the format Babel, ESLint, and most JS tooling expect).
        let ast_json = self.with_program(handle, |program| {
            crate::estree::program_to_estree(program).to_string()
        }).ok_or("Failed to access AST for serialization")?;

        Ok(PluginAst {
            source: source.to_string(),
            ast_json,
            kind: kind.to_string(),
            file_path: file_path.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_handle_is_deterministic() {
        let h1 = AstHandle::from_source("const x = 1;");
        let h2 = AstHandle::from_source("const x = 1;");
        assert_eq!(h1, h2);
    }

    #[test]
    fn ast_handle_differs_for_different_source() {
        let h1 = AstHandle::from_source("const x = 1;");
        let h2 = AstHandle::from_source("const y = 2;");
        assert_ne!(h1, h2);
    }

    #[test]
    fn ast_pool_caches_parse() {
        let mut pool = AstPool::new();
        let source = "export const x = 1;";

        let h1 = pool.get_or_parse(source, SourceType::mjs()).unwrap();
        assert_eq!(pool.len(), 1);

        // Same source — should be a cache hit
        let h2 = pool.get_or_parse(source, SourceType::mjs()).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(pool.len(), 1); // No new entry
    }

    #[test]
    fn ast_pool_with_program_read() {
        let mut pool = AstPool::new();
        let source = "const x = 1; const y = 2;";
        let handle = pool.get_or_parse(source, SourceType::mjs()).unwrap();

        let stmt_count = pool.with_program(handle, |program| program.body.len());
        assert_eq!(stmt_count, Some(2));
    }

    #[test]
    fn ast_pool_take_ownership() {
        let mut pool = AstPool::new();
        let source = "const x = 1;";
        let handle = pool.get_or_parse(source, SourceType::mjs()).unwrap();

        let (allocator, program) = pool.take(handle).unwrap();
        assert_eq!(program.body.len(), 1);
        assert!(!pool.contains(handle));
        // Allocator must stay alive while program is used
        drop(allocator);
    }

    #[test]
    fn ast_pool_eviction() {
        let mut pool = AstPool::with_capacity(2);

        let h1 = pool.get_or_parse("const a = 1;", SourceType::mjs()).unwrap();
        let h2 = pool.get_or_parse("const b = 2;", SourceType::mjs()).unwrap();
        assert_eq!(pool.len(), 2);

        // Adding a third should evict the oldest (h1)
        let h3 = pool.get_or_parse("const c = 3;", SourceType::mjs()).unwrap();
        assert_eq!(pool.len(), 2);
        assert!(!pool.contains(h1));
        assert!(pool.contains(h2));
        assert!(pool.contains(h3));
    }

    #[test]
    fn ast_pool_clear() {
        let mut pool = AstPool::new();
        pool.get_or_parse("const x = 1;", SourceType::mjs()).unwrap();
        pool.get_or_parse("const y = 2;", SourceType::mjs()).unwrap();
        assert_eq!(pool.len(), 2);

        pool.clear();
        assert!(pool.is_empty());
    }

    #[test]
    fn ast_pool_with_program_mut() {
        let mut pool = AstPool::new();
        let source = "const x = 1;";
        let handle = pool.get_or_parse(source, SourceType::mjs()).unwrap();

        pool.with_program_mut(handle, |program| {
            // Mutate the program (e.g., add a statement)
            // We're just verifying mutable access works
            assert!(!program.body.is_empty());
        });
    }

    #[test]
    fn ast_pool_parse_once_for_multiple_consumers() {
        // Simulate the parse-once pattern: parse once, run multiple read-only
        // visitors, then take for transformation.
        let mut pool = AstPool::new();
        let source = "import('./dynamic'); t('hello'); const x = 1;";
        let handle = pool.get_or_parse(source, SourceType::mjs()).unwrap();

        // Consumer 1: dynamic import detection (read-only)
        let imports = pool.with_program(handle, |prog| {
            crate::transform::detect_dynamic_imports_from_program(prog)
        }).unwrap();
        assert_eq!(imports, vec!["./dynamic".to_string()]);

        // Consumer 2: i18n key extraction (read-only)
        let i18n = pool.with_program(handle, |prog| {
            crate::i18n::extract_i18n_keys_from_program(prog, source, "test.tsx")
        });
        assert!(i18n.is_some());
        let i18n = i18n.unwrap();
        assert_eq!(i18n.keys.len(), 1);
        assert_eq!(i18n.keys[0].key, "hello");

        // Consumer 3: take for transformation (removes from pool)
        let (allocator, program) = pool.take(handle).unwrap();
        assert_eq!(program.body.len(), 3); // import, call, const
        assert!(!pool.contains(handle));
        drop(allocator);
    }

    #[test]
    fn ast_pool_handle_collision_resistance() {
        // Verify that different sources get different handles
        let sources = [
            "const a = 1;",
            "const b = 2;",
            "const c = 3;",
            "export default function() {}",
            "import x from 'y';",
        ];
        let handles: Vec<AstHandle> = sources
            .iter()
            .map(|s| AstHandle::from_source(s))
            .collect();

        // All handles should be unique
        for i in 0..handles.len() {
            for j in (i + 1)..handles.len() {
                assert_ne!(handles[i], handles[j], "Collision between sources {} and {}", i, j);
            }
        }
    }

    #[test]
    fn plugin_ast_source_provides_summary() {
        let mut pool = AstPool::new();
        let source = r#"
            import React from "react";
            const dyn = import("./dynamic");
            export const foo = 42;
        "#;

        let ast = pool.get_plugin_ast(source, "test.tsx", "tsx").unwrap();
        assert_eq!(ast.source, source);
        assert_eq!(ast.kind, "tsx");
        assert_eq!(ast.file_path, "test.tsx");

        // The summary should contain the dynamic import
        let parsed: serde_json::Value = serde_json::from_str(&ast.ast_json).unwrap();
        assert_eq!(parsed["type"], "Program");
        let dynamic_imports = parsed["dynamicImports"].as_array().unwrap();
        assert!(dynamic_imports.contains(&serde_json::Value::String("./dynamic".to_string())));
    }

    #[test]
    fn plugin_ast_source_caches_parse() {
        let mut pool = AstPool::new();
        let source = "const x = 1;";

        // First call — parses and caches
        pool.get_plugin_ast(source, "test.js", "js").unwrap();
        assert_eq!(pool.len(), 1);

        // Second call — should hit cache (no new entry)
        pool.get_plugin_ast(source, "test.js", "js").unwrap();
        assert_eq!(pool.len(), 1, "Second call should hit cache");
    }
}
