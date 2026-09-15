# PledgePack Plugin ABI — WIT Contract v0.1.2

> **FROZEN**: This contract is frozen at v0.1.2. Breaking changes require a new world (`pledgepack-plugin-v1`). Additive changes (new hooks, new optional fields) are allowed within v0.1.x.

## What This Is

The WIT (WASM Interface Type) contract defines the plugin ABI for PledgePack. This is the **one-way door** — once plugins are written against this contract, breaking it nukes the ecosystem. It deserves 10x the design care of anything else.

## Two-Tier Plugin System

| Tier | ABI | Cache | Sandbox | Target |
|---|---|---|---|---|
| **First-class** | WASM component (this WIT contract) | Fine-grained — hook output is a cached task node | Yes (WASM sandbox) | Plugin authors who want speed + cache + portability |
| **Second-class** | JS shim (QuickJS/V8) | Coarse — opaque blob cached at module level | No (JS runtime) | Existing Vite/Rollup plugin ecosystem |

Both tiers implement the same hook shapes. The JS shim wraps Vite/Rollup plugins and translates their output to the WIT contract's types.

## Hooks

### Module Pipeline (sequential)

| Hook | Input | Output | Semantics |
|---|---|---|---|
| `resolve-id` | source, importer, is-entry, kind | id, external, cache-key | First non-null wins |
| `load` | id | code, map, cache-key | First non-null wins |
| `transform` | code, id, ast-json? | code, map, cache-key | Chain — each plugin sees previous output |

### HTML Transform (sequential)

| Hook | Input | Output | Semantics |
|---|---|---|---|
| `transform-index-html` | html, path | html, tags[], cache-key | Chain |

### Render Chunk (sequential)

| Hook | Input | Output | Semantics |
|---|---|---|---|
| `render-chunk` | code, filename, chunk-type | code, map, cache-key | Chain — each plugin sees previous output |

### Lifecycle (parallel)

| Hook | Input | Output | Semantics |
|---|---|---|---|
| `build-start` | — | void | All plugins called |
| `build-end` | — | void | All plugins called |
| `generate-bundle` | — | void | All plugins called |

### Dev Server (dev mode only)

| Hook | Input | Output | Semantics |
|---|---|---|---|
| `configure-server` | — | server-middleware? | Returns middleware to register |

## Cache Contract

Every hook output includes a `cache-key: string` field. This is a hash of all inputs that affect the result. The host uses this to cache the hook output as a task graph node (`Task<T>`).

- **WASM plugins**: The plugin computes the cache key from its inputs. The host trusts this key for caching.
- **JS shim plugins**: The shim computes a coarse cache key (hash of input code + plugin path). The plugin's internal reads are opaque, so invalidation is coarse.
- **Cache key format**: Hex-encoded blake3 hash (64 chars). The host may use a different hash internally but the contract specifies blake3 for cross-plugin consistency.

## AST Access (Phase 0/2 Extension)

The `transform` hook includes an optional `ast-json` field in its input. This is the pre-parsed AST as ESTree-compatible JSON. Present only if:
1. The plugin declares `needs-ast: true` in a future metadata extension, AND
2. The host supports AST serialization (requires `oxc/serde` or an ESTree converter)

Plugins **must** handle `ast-json: none` gracefully (fall back to parsing the code themselves).

In v0.1.x, `ast-json` is always `none`. This field is a forward-compatible extension point.

## Versioning Policy

### v0.1.2 (this version) — FROZEN

- 9 hooks: resolve-id, load, transform, transform-index-html, render-chunk, build-start, build-end, generate-bundle, configure-server
- All outputs include cache-key and `schema-version`
- AST access via `ast-json` field (always `none` in v0.1.x)
- `render-chunk` hook added (additive, backward-compatible)
- `schema-version: option<u32>` field on all output records (G7.6)

### Additive changes allowed in v0.1.x

- New hooks (e.g., `render-chunk`, `write-bundle`, `close-bundle`)
- New optional fields on existing records
- New hook flags in `hook-flags`

### Breaking changes require v1

- Removing or renaming a hook
- Changing a field type
- Changing hook semantics (e.g., from first-wins to chain)
- Removing a field from a record

## Mapping to Vite/Rollup

| Vite/Rollup Hook | WIT Hook | Notes |
|---|---|---|
| `resolveId(source, importer, options)` | `resolve-id(input)` | `options.isEntry` → `is-entry`, `options.kind` → `kind` |
| `load(id)` | `load(input)` | Same semantics |
| `transform(code, id)` | `transform(input)` | Added `ast-json` extension |
| `transformIndexHtml(html)` | `transform-index-html(input)` | Added `tags` in output |
| `renderChunk(code, chunk, outputOptions)` | `render-chunk(input)` | Added in v0.1.2 — `chunk.name` → `filename`, `chunk.type` → `chunk-type` |
| `buildStart()` | `build-start()` | Same |
| `buildEnd()` | `build-end()` | Same |
| `generateBundle()` | `generate-bundle()` | Same |
| `configureServer(server)` | `configure-server()` | Returns middleware source (no server object passed) |

### Intentional differences from Vite

1. **No `config`/`configResolved` hooks** — PledgePack config is resolved before plugins load. Plugins read config via a future `get-config` host import (not in v0.1.0).
2. **No `writeBundle`/`closeBundle`** — deferred to v0.1.x (additive). `renderChunk` was added in v0.1.2.
3. **No `handleHotUpdate`/`watchChange`** — HMR is handled internally by PledgePack's dev server. Plugin HMR hooks may be added in v0.1.x.
4. **`configure-server` returns middleware source** — instead of receiving a server object to mutate, the plugin returns middleware source code. This is sandbox-friendly (no object references cross the WASM boundary).
5. **`cache-key` in every output** — Vite/Rollup don't have this. It's the PledgePack moat: every hook output is a cacheable task node.

## File Structure

```
wit/
  world.wit          — the world definition (all hooks + types inline)
  README.md          — this file
```

Future structure (when splitting into interfaces):
```
wit/
  pledgepack/
    plugin/
      world.wit      — world definition (imports interfaces)
      types.wit      — shared types
      transform.wit  — transform interface
      resolve.wit    — resolve-id interface
      load.wit       — load interface
      hooks.wit      — lifecycle + dev server hooks
```

## Validation

The WIT contract can be validated with:
```bash
wit-parser wit/ --feature plugin
```

Or using `wasm-tools component`:
```bash
wasm-tools component new plugin.wasm -d wit/ --output plugin-component.wasm
```
