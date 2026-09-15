# Writing PledgePack plugins

PledgePack has two plugin hosts. Both implement the same hook contract
(defined in [`wit/world.wit`](../wit/world.wit), currently frozen at
**v0.1.3**) and, as of this document, both genuinely execute every hook —
see [Hook support matrix](#hook-support-matrix) below for how that's
verified, not just claimed. See PRODUCTION-READINESS-100.md goal 48.

| | `wasm-plugin-host` | `js-plugin-host` |
|---|---|---|
| Runtime | `wasmtime` (WASM Component Model) | QuickJS (via `rquickjs`) |
| Sandboxing | Yes — no filesystem/network by default, CPU fuel limit, memory cap | No — runs with the host process's own privileges |
| Plugin format | A compiled `.wasm` component implementing the `pledgepack-plugin` world | A `.js`/`.ts` file exporting a plugin object (Vite/Rollup-shaped) |
| Recommended for | New plugins, anything untrusted or third-party | Fast local iteration, porting an existing Vite/Rollup plugin |

If you don't have a reason to pick one, use `wasm-plugin-host` — it's the
one PledgePack is consolidating on (see "Which host should I target?"
below).

> ⚠️ **Known issue as of this writing:** the pinned `wasmtime`/`wasmtime-wasi`
> version (28.0.1) has multiple disclosed RUSTSEC advisories, some of them
> sandbox-escape or memory-safety issues in the WASM runtime itself — see
> `PRODUCTION-READINESS-100.md`'s Phase 1 callout. Until that's resolved,
> "sandboxed" in the table above is the *design intent*, not a guarantee to
> rely on for genuinely untrusted, adversarial plugin code.

## Hook reference

| Hook | Signature | Ordering | Notes |
|---|---|---|---|
| `resolveId` | `(source, importer, isEntry, kind) -> id?` | Sequential, first non-null wins | |
| `load` | `(id) -> code?` | Sequential, first non-null wins | |
| `transform` | `(code, id) -> code?` | Sequential chain — each plugin sees the previous plugin's output | |
| `transformIndexHtml` | `(html, path) -> html?` | Sequential chain | |
| `renderChunk` | `(code, filename, chunkType) -> code?` | Sequential chain | Runs after code splitting, before final emit. Added to the WIT contract in v0.1.2. |
| `handleHotUpdate` | `(file, timestamp) -> moduleIds?` | Sequential, first non-null wins | Dev mode only. Returning `none` defers to the next plugin (or the host's default HMR resolution if none handle it); returning an *empty* `moduleIds` list means "I handled this, suppress HMR for it" — those are different outcomes, pick deliberately. Added to the WIT contract in v0.1.3 — previously absent from both hosts. |
| `buildStart` | `()` | All plugins called; order not guaranteed | |
| `buildEnd` | `()` | All plugins called; order not guaranteed | |
| `generateBundle` | `()` | All plugins called; order not guaranteed | |
| `configureServer` | `() -> middleware?` | All plugins called, results collected | Dev mode only. |

**Plugin ordering** (`enforce`): a plugin can declare `enforce: "pre"` to run
before PledgePack's built-in transform, or `enforce: "post"` (the default,
including when `enforce` is unset) to run after. This only affects
`transform`'s position relative to the built-in transform pipeline — it
doesn't reorder plugins relative to each other within the "pre" or "post"
group.

**Batch-load failure semantics differ between hosts** — worth knowing if
you're loading several plugins at once:
- `wasm-plugin-host`'s `load_plugins()` is fail-fast: the first plugin that
  fails to load aborts the whole call, and no later plugins in the batch
  are loaded either.
- `js-plugin-host`'s `load_plugins()` skips a plugin that fails to
  evaluate (logging a loud `error!`, not a `warn!`) and continues loading
  the rest of the batch.

Neither silently registers a broken plugin as loaded (see
PRODUCTION-READINESS-100.md goal 49) — but if you're loading plugins in
bulk and want "one bad plugin doesn't stop the others" behavior specifically,
that's `js-plugin-host`'s behavior today, not `wasm-plugin-host`'s.

## Hook support matrix

Both hosts expose a `hook_support_matrix()` function (and the underlying
`host_supports_hook(name)` check) returning, for every hook name in
`pledgepack_core::plugin_system::PLUGIN_HOOK_NAMES` (the canonical list,
kept in sync with `wit/world.wit` by hand), whether that host actually
executes it. Both crates' test suites assert the matrix has no gaps —
`hook_support_matrix_has_no_gaps` in each of `wasm-plugin-host`'s and
`js-plugin-host`'s `#[cfg(test)]` modules. That test is the thing that would
have caught the `buildStart`/`buildEnd`/`generateBundle` bug (hooks that
were *detected* on a plugin but never actually *executed*) before it
shipped — call `hook_support_matrix()` yourself if you want to verify this
for your own build rather than trusting this document.

## Which host should I target?

PledgePack is consolidating on `wasm-plugin-host` (the WASM Component Model
host) as the primary target, because:
- It has stronger sandboxing by design (once the wasmtime CVE issue above is
  resolved).
- It had full hook parity with `js-plugin-host` first, and both hosts'
  parity is now mechanically checked (see above) rather than hoped for.

`js-plugin-host` isn't deprecated or scheduled for removal — it remains a
real, fully-functional path, useful for fast local iteration or porting an
existing Vite/Rollup plugin without a WASM toolchain. Don't assume it will
be removed; if that changes, this document (and `wit/world.wit`'s
changelog) will say so explicitly.

## Porting a JS plugin to WASM

`pledge plugin migrate` (backed by
`pledgepack_js_plugin_host::advanced::generate_wasm_skeleton`) generates a
**signature scaffold**, not a working port: it detects which hooks your JS
plugin declares and emits a matching Rust function signature per hook, each
body a `todo!()`. It does not, and cannot in general, translate your JS
hook logic into Rust automatically — the generated file's own header
comment says so, and every stub panics via `todo!()` rather than silently
compiling as a no-op, specifically so an unfinished port fails loudly the
moment it's exercised instead of shipping as "migrated" while actually
doing nothing.
