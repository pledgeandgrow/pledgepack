# PledgePack Benchmarks

> **2026-09-15 correction — the previous version of this document has been
> removed.** It presented specific, precise-looking comparative numbers
> against Vite, Turbopack, esbuild, and webpack (e.g. "cold start ~45ms vs
> Vite's ~320ms", "HMR latency ~8ms") formatted as measured results, with a
> methodology section claiming "5 runs, median reported" on specific
> hardware. **These numbers were not produced by any benchmark tooling that
> exists in this codebase.** The only real benchmark tooling here —
> `pledgepack bench` (measures PledgePack's own build time against its own
> previous runs) and `zig build bench` (Zig-side micro-benchmarks of the
> native module graph, SIMD scanning, and file I/O) — has no code path that
> invokes or measures Vite, Turbopack, esbuild, or webpack; nothing in this
> repository could have produced a Vite-vs-PledgePack comparison. The
> document also contradicted itself on which version was tested ("PledgePack
> v0.2.8" in the header, "v0.1.8" in the methodology section) — real
> measured data doesn't have that inconsistency. Given this, and separately,
> that the compiled `pledge` binary segfaulted on every invocation until a
> fix landed in this same session (see
> [`PRODUCTION-READINESS-100.md`](PRODUCTION-READINESS-100.md)'s "Known
> blockers"), there is no realistic way those specific numbers were measured
> on a working build recently. Treating fabricated numbers as real
> comparative data is worse than having no benchmark doc at all, so it's
> been deleted rather than kept or "corrected" — there's nothing accurate
> underneath it to correct.

## What's real

Two genuine, runnable benchmark tools exist in this codebase today:

- **`pledgepack bench`** — runs the current project's build N times and reports
  min/max/avg/median wall-clock time, optionally against a saved baseline
  (`--baseline <ref>`, `--save`). This measures PledgePack's own build
  performance over time (e.g. did a change regress build speed), not a
  comparison against other tools.
- **`zig build bench`** — the Zig-side micro-benchmark suite
  (`native-sys/zig/bench.zig`) for the module graph, SIMD source scanning,
  and batch file I/O implemented in Zig. Also internal-only, no comparative
  numbers against other tools.
- **`cargo bench -p pledgepack-core`** — criterion benchmarks for the core
  transform pipeline (`crates/core/benches/transform_bench.rs`), added
  2026-09-15 (`PRODUCTION-READINESS-100.md` Phase 9 goal 98), tracked in CI
  as a build artifact per push. Also internal-only.

None of these three currently compare against Vite, Turbopack, esbuild, or
webpack. Producing an honest comparative benchmark would require actually
building equivalent test projects in each tool and running them
side-by-side under controlled conditions — real work that hasn't been done
yet, tracked as open follow-up rather than filled in with invented numbers.

## Architectural characteristics (verifiable from source, not measurements)

These are real, source-verifiable design choices — worth stating on their
own terms, without attaching fabricated timing numbers to them:

- **No Node.js runtime in the built binary** — `pledge` is a native
  Rust+Zig binary; there's no V8/Node.js boot cost paid at startup the way
  a Node-hosted bundler pays it.
- **Zig for hot paths** — file I/O, the module graph, and SIMD-accelerated
  import scanning are implemented in Zig and called via a C ABI from Rust
  (`native-sys/`), rather than in JavaScript or pure Rust.
- **Arena-allocated module graph** — the Zig module graph
  (`native-sys/zig/graph.zig`) uses arena allocation rather than per-node
  heap allocation with reference counting.
- **Oxc for JS/TS/JSX** — parsing, transformation, and codegen go through
  `oxc`, a Rust-native toolchain, avoiding a Rust↔JS boundary for the
  transform hot path.
- **Content-hash-based caching** — `crates/cache/` keys cache entries by
  content hash (via `blake3`) rather than file-modification timestamps.

None of these claims are, on their own, a performance number — they're
structural facts about how the code is built. Whether they add up to
"faster than Vite" in practice is an empirical question this document no
longer claims an answer to until it's actually measured.
