# PledgePack ↔ PledgeStack — Architecture Connection

## Project Split

### PledgePack (Bundler / Build Tool)
- **Role:** Framework-agnostic bundler, dev server, and build tool (like Turbopack/esbuild/SWC)
- **Repository:** `https://github.com/pledgeandgrow/pledgepack`
- **npm package:** `pledgepack` (latest published: `0.4.0`, `latest` dist-tag — tagged `v0.4.0` in this repo)
- **Binary:** Native Rust binary (`pledge.exe` / `pledge`) distributed via GitHub Releases + postinstall download
- **Language:** Rust (Oxc parser, Lightning CSS, QuickJS JS runtime for plugin host and tests, wasmtime for WASM plugins)
- **CLI:** `pledgepack dev`, `pledgepack build`, `pledgepack serve`, `pledgepack preview`, `pledgepack test`, `pledgepack analyze`, `pledgepack create`, `pledgepack init`, `pledgepack migrate`, `pledgepack doctor`, `pledgepack bench`, `pledgepack cache`, `pledgepack clean`, `pledgepack config`, `pledgepack update`, `pledgepack why`, `pledgepack schema`, `pledgepack generate-env-types`, `pledgepack completions`, `pledgepack manpages`, `pledgepack dashboard`, `pledgepack playground`, `pledgepack plugin` (`create`/`docs`/`install`/`keygen`/`list`/`search`/`sign`)

### PledgeStack (React Framework)
- **Role:** Opinionated React framework with SSR/SSG/RSC, file-based routing, API routes (like Next.js is to Turbopack)
- **Repository:** `https://github.com/pledgeandgrow/pledgestack` (monorepo)
- **npm package:** `pledgestack` (repo version `0.2.0`; latest published on npm is `0.1.12` — all 34 public packages share one version via a Changesets fixed group)
- **Language:** TypeScript/JavaScript (depends on pledgepack binary)
- **CLI:** `pledge dev`, `pledge build`, `pledge start` (`dev`/`build` spawn the compiled `pledgepack` binary directly by resolved path, not via a shell command — see "CLI Command Mapping" below)

---

## Dependency Relationship

```
User installs pledgestack (framework)
  └── pledgestack depends on pledgepack (bundler)
       └── pledgepack postinstall downloads native binary from GitHub Releases
```

**pledgestack `package.json`:**
```json
{
  "dependencies": {
    "pledgepack": "^0.4.0"
  }
}
```

> **Note (2026-09-23):** PledgeStack's dependency range is `^0.4.0` in all
> four pledgejs `package.json`s (plus the `minimumReleaseAgeExclude` pin in
> `pnpm-workspace.yaml`), matching `pledgepack@0.4.0` — published to npm and
> resolvable (`latest = 0.4.0`).

---

## Responsibility Split

| Concern | PledgePack | PledgeStack |
|---------|-----------|----------|
| Module bundling | ✅ | |
| Tree shaking / code splitting | ✅ | |
| Dev server (HTTP, WebSocket, HMR) | ✅ | |
| Transform pipeline (JS/TS/JSX/CSS) | ✅ | |
| Asset pipeline (images, fonts, SVG, MDX) | ✅ | |
| Plugin system (JS via QuickJS, WASM via wasmtime) | ✅ | |
| Output formats (ESM, CJS, IIFE, edge) | ✅ | |
| Source maps | ✅ | |
| CSS processing (Tailwind, CSS Modules, Lightning CSS) | ✅ | |
| Test runner (Vitest-compatible) | ✅ | |
| Bundle analyzer | ✅ | |
| Cache (memory, disk, remote) | ✅ | |
| File-based routing (`app/` directory) | | ✅ |
| Layouts, error boundaries, loading states | | ✅ |
| SSR / SSG / ISR rendering | | ✅ |
| React Server Components (RSC) | | ✅ |
| Data fetching (`cachedFetch`, `serverCachedFetch`, `unstable_cache`) | | ✅ |
| API routes (`app/api/*/route.ts`) | | ✅ |
| Server actions (`serverAction()`) | | ✅ |
| Head / metadata management | | ✅ |
| `<PledgeLink>`, `<PledgeImage>`, `<PledgeHead>` components | | ✅ |
| Instrumentation lifecycle hooks (`loadInstrumentation`) | | ✅ |
| Static export mode (`generateStaticExport`) | | ✅ |
| Framework conventions and types | | ✅ |
| Production SSR server | | ✅ |

---

## What PledgePack MUST NOT Handle (leave to PledgeStack)

PledgePack is a **dumb bundler** — it transforms files and serves them. It does NOT know about React, routing semantics, or server rendering:

- **DO NOT** implement React-specific rendering logic (JSX → HTML string, hydration scripts)
- **DO NOT** implement route matching or route params (PledgePack scans `app/` for files, but route matching at runtime is PledgeStack)
- **DO NOT** implement SSR server (Node.js HTTP server that renders React on request)
- **DO NOT** implement SSG page generation (calling React `renderToString` per route)
- **DO NOT** implement ISR (re-validation logic, stale-while-revalidate cache)
- **DO NOT** implement React Server Components protocol (RSC payload serialization/deserialization)
- **DO NOT** implement API route handlers (PledgePack provides the mechanism, PledgeStack provides the handler)
- **DO NOT** implement server actions (`serverAction()` function — PledgeStack only)
- **DO NOT** implement data fetching patterns (`cachedFetch`, `serverCachedFetch`, `unstable_cache` — PledgeStack only)
- **DO NOT** implement `<PledgeLink>`, `<PledgeImage>`, `<PledgeHead>`, `<ErrorBoundary>` components
- **DO NOT** implement metadata/SEO management (`<meta>` tag injection, Open Graph, sitemaps)
- **DO NOT** implement i18n routing (locale detection, locale-prefixed routes)
- **DO NOT** implement authentication middleware (session, cookies, JWT)
- **DO NOT** implement production Node.js server (`pledge start` — this is PledgeStack only)
- **DO NOT** implement framework-specific config (PledgePack only reads `pledge.config.ts`)
- **DO NOT** implement Next.js-compatible APIs (`getStaticProps`, `getServerSideProps` — PledgeStack wraps these)
- **DO NOT** implement HTML template generation for SSR (PledgeStack provides the HTML shell, PledgePack just processes it)
- **DO NOT** implement instrumentation lifecycle hooks (`loadInstrumentation` — PledgeStack only)
- **DO NOT** implement static export mode (`generateStaticExport` — PledgeStack only)

**What PledgePack DOES provide for PledgeStack to build on:**
- `appDir` config field → scans `app/` directory, generates `__pledge_router` virtual module with route table
- Plugin hooks (`resolveId`, `load`, `transform`, `transformIndexHtml`, `configureServer`, `buildStart`, `buildEnd`, `generateBundle`) → PledgeStack plugins use these
- Dev server middleware plugin hook (`configureServer`) → PledgeStack injects SSR/API route middleware
- `ssr` config field → tells PledgePack to preserve server entry for SSR
- HTML processing (`html.rs`) → processes `<script>` and `<link>` tags in HTML entry
- Edge bundle generation → outputs edge-compatible bundle for Cloudflare/Vercel

---

## What PledgeStack MUST NOT Handle (leave to PledgePack)

PledgeStack is a **framework layer** — it orchestrates React rendering and routing. It does NOT do bundling or file transformation:

- **DO NOT** implement module bundling (concatenating modules, resolving imports, chunk splitting)
- **DO NOT** implement JS/TS/JSX transformation (Oxc parser, syntax lowering, JSX → JS)
- **DO NOT** implement CSS processing (Lightning CSS, Tailwind, CSS Modules, SCSS/SASS/LESS)
- **DO NOT** implement tree shaking (dead code elimination, side-effect detection)
- **DO NOT** implement code splitting (chunk graph, shared chunks, dynamic imports)
- **DO NOT** implement source map generation
- **DO NOT** implement asset pipeline (image optimization, font subsetting, SVG sprites, MDX)
- **DO NOT** implement HMR WebSocket (PledgePack handles WebSocket, HMR diff, module invalidation)
- **DO NOT** implement file watcher (PledgePack uses native inotify/FSEvents/ReadDirectoryChangesW)
- **DO NOT** implement build cache (memory cache, disk cache, remote cache, git-based invalidation)
- **DO NOT** implement test runner (PledgePack has full Vitest-compatible runner with QuickJS engine)
- **DO NOT** implement bundle analyzer (PledgePack generates interactive HTML treemap)
- **DO NOT** implement output format conversion (ESM → CJS/IIFE/UMD)
- **DO NOT** implement compression (gzip/brotli output generation)
- **DO NOT** implement plugin sandboxing (JS plugin limits, filesystem access control)
- **DO NOT** implement dependency pre-bundling (DepBundler in PledgePack handles this)
- **DO NOT** implement polyfills (PledgePack has 20 built-in Node.js polyfills)
- **DO NOT** implement define/compile-time constants (PledgePack handles `define` config)
- **DO NOT** implement binary distribution (PledgePack handles its own native binary via postinstall)
- **DO NOT** implement migration tooling (PledgePack migrates from Vite/webpack/Turbopack configs)
- **DO NOT** implement LSP server (PledgePack has built-in LSP for import resolution and diagnostics)

**What PledgeStack DOES provide on top of PledgePack:**
- `pledge.config.ts` → user-facing framework config (SSR, i18n, images, experimental features)
- React components: `<PledgeLink>`, `<PledgeImage>`, `<PledgeHead>`, `<ErrorBoundary>`, `<Loading>`
- Server runtime: Node.js HTTP server for `pledge start` (production SSR via `startNodeServer`)
- Edge runtime: `createEdgeHandler` for Cloudflare Workers / Vercel Edge / Deno Deploy
- Route matching: interprets `__pledge_router` virtual module, matches URLs to route components
- SSR rendering: `renderSSR`, `renderSSRStream` with layout chains, error boundaries, Suspense
- RSC rendering: `renderRSCToHTML`, `renderRSCStream` (no `renderRSC` — removed)
- SSG generation: `generateStaticExport` (full export mode), `generateStaticPages` (incremental SSG)
- API routes: resolves `app/api/*/route.ts` files, calls handlers for matching requests
- Server actions: `serverAction()` function with automatic client→server RPC via POST endpoint
- Data fetching: `cachedFetch`, `serverCachedFetch`, `unstable_cache`, `revalidateTag`, `revalidatePath`
- Instrumentation: `loadInstrumentation` loads `instrumentation.ts` at server startup, calls `register()`
- Server utilities: `cookies()`, `headers()`, `searchParams()`, `params()`, `redirect()`, `notFound()`, `draftMode()`, `after()`

---

## Integration Points

### 1. PledgeStack calls PledgePack via CLI

PledgeStack CLI wraps the `pledge` binary:

```typescript
// PledgeStack CLI (simplified)
import { runPledgepack } from 'pledgepack';

// dev command
await runPledgepack(['dev', '--port', '3000']);

// build command
await runPledgepack(['build']);

// build with SSG
await runPledgepack(['build', '--ssg']);
```

### 2. PledgeStack generates pledge.config.ts

PledgeStack auto-generates or extends the PledgePack config:

```typescript
// PledgeStack generates this pledge.config.ts
import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['app/entry.tsx'],
  framework: 'react',
  appDir: 'app',
  devServer: {
    port: 3000,
    hmr: true,
  },
  plugins: [
    // PledgeStack injects its own plugins for RSC, SSR, routing
    { name: 'pledgestack-rsc', resolve: './plugins/rsc.js' },
    { name: 'pledgestack-ssr', resolve: './plugins/ssr.js' },
    { name: 'pledgestack-router', resolve: './plugins/router.js' },
  ],
});
```

#### Shared config file contract

PledgeStack and PledgePack read the same `pledge.config.ts`. PledgePack
validates the file against its generated schema and warns on unknown fields —
to keep framework-level keys (e.g. `rsc`, `tailwind`, `ppr`, `rateLimit`,
`cors`, `appDir`, `rootDir`, `output`) legal in the shared file, PledgePack
treats them as a reserved extension namespace (`PLEDGESTACK_FIELDS` in
`crates/core/src/config_validate.rs`) and never warns on them, whatever
`framework` is set to ('pledge', 'react', 'vue', … or unset).

When PledgeStack adds a new top-level config field, add the key to
`PLEDGESTACK_FIELDS` (single source of truth for the contract) — do not add it
to `PledgeConfig`.

### 3. PledgePack plugin hooks for PledgeStack

PledgePack exposes these plugin hooks that PledgeStack plugins use:

```
buildStart       — called before first transform
resolveId        — intercept import resolution (for virtual modules like __pledge_router)
load             — provide virtual module content
transform        — modify source code (for RSC serialization, SSR transforms)
renderChunk      — modify chunk content before emit
generateBundle   — add extra files to output (SSG HTML pages, sitemap)
writeBundle      — post-build actions (submit to search engines, etc.)
```

### 4. Virtual modules

PledgePack already supports virtual modules. PledgeStack uses these:

| Virtual module | Purpose |
|---------------|---------|
| `__pledge_router` | Auto-generated router from `app/` directory |
| `__pledge_manifest` | Build manifest for SSR asset injection |
| `__pledge_rsc_client` | RSC client renderer |
| `__pledge_rsc_server` | RSC server renderer |

### 5. PledgePack config fields PledgeStack relies on

```typescript
{
  appDir: 'app',              // file-based routing directory
  framework: 'react',         // JSX transform mode
  entry: ['app/entry.tsx'],   // entry points
  htmlEntry: 'index.html',    // HTML template
  sourceMaps: true,           // source maps for dev
  plugins: [...],             // PledgeStack plugins
  ssr: {                      // SSR config
    entry: 'app/entry.server.tsx',
    runtime: 'node',
  },
  edgeTarget: 'cloudflare',   // edge deployment
}
```

---

## Binary Distribution

### PledgePack binary
- Built in Rust, cross-compiled for 6 platform targets via CI (Windows x64, Linux x64/arm64, macOS x64/arm64, Windows ARM64)
- Distributed via GitHub Releases: `https://github.com/pledgeandgrow/pledgepack/releases/latest/download/pledge-{target}.{ext}`
- `postinstall.js` downloads the correct binary automatically
- JS shim (`bin/pledge.js`) resolves binary from:
  1. `target/release/` (dev mode)
  2. `target/debug/` (dev mode)
  3. `bin/{platform-key}/` (downloaded by postinstall)
  4. `bin/platform/{platform-key}/` (CI staged)
  5. `bin/` (direct install)

### PledgeStack does NOT have its own binary
- PledgeStack is pure TypeScript/JavaScript
- It spawns the `pledge` binary (from PledgePack) for all build operations
- PledgeStack adds framework middleware on top of PledgePack's dev server

---

## Package Structure

### PledgePack (published from pledgepack repo)
```
pledgepack/
├── bin/
│   ├── pledge.js          # CLI shim (resolves native binary)
│   └── postinstall.js     # Downloads binary from GitHub Releases
├── index.js               # JS entry — exports `defineConfig` (typed config helper only)
├── index.d.ts             # Generated config types (`pnpm gen:types`, from `pledgepack schema`)
├── package.json           # name: "pledgepack", version: "0.4.0"
├── README.md
└── LICENSE
```

### PledgeStack (published from pledgeandgrow/pledgestack)
```
pledgestack/
├── packages/
│   ├── cli/                       # Main framework package — published as `pledgestack` on npm
│   │   ├── src/
│   │   │   ├── commands/          # CLI commands (dev, build, start, create, info, doctor)
│   │   │   ├── config-loader.ts   # Loads pledge.config.ts
│   │   │   ├── index.ts           # Re-exports all sub-packages
│   │   │   └── ...
│   │   ├── scripts/build.mjs      # esbuild bundler (bundles all sub-packages into dist/)
│   │   ├── package.json           # name: "pledgestack", deps: { pledgepack: "^0.4.0" }
│   │   └── README.md
│   ├── shared/                    # Public (`pledgestack-shared`) — also bundled into CLI via esbuild
│   ├── core/                      # Public (`pledgestack-core`) — bundled into CLI
│   ├── server/                    # Public (`pledgestack-server`) — bundled into CLI
│   ├── client/                    # Public (`pledgestack-client`) — bundled into CLI
│   ├── auth/                      # Public (`pledgestack-auth`) — bundled into CLI
│   ├── state/                     # Public (`pledgestack-state`) — bundled into CLI
│   ├── api/                       # Public (`pledgestack-api`) — bundled into CLI
│   ├── a11y/                      # Public (`pledgestack-a11y`) — bundled into CLI
│   ├── overlay/                   # Public (`pledgestack-overlay`) — bundled into CLI
│   ├── seo/                       # Public (`pledgestack-seo`) — bundled into CLI
│   └── ...                        # 34 public packages total (one shared version); only the two VS Code extensions are private
```

---

## Config Flow

```
User writes pledge.config.ts          PledgeStack reads it
         │
         ▼
PledgeStack extends pledge.config.ts     PledgePack reads it
         │
         ▼
PledgePack runs build/dev/test          native binary executes
```

### pledge.config.ts (user-facing, framework-specific)
```typescript
import { defineConfig } from 'pledgestack';

export default defineConfig({
  ssr: true,
  ssg: false,
  isr: { revalidate: 60 },
  i18n: {
    locales: ['en', 'fr'],
    defaultLocale: 'en',
  },
  images: {
    domains: ['cdn.example.com'],
  },
  experimental: {
    serverActions: true,
    rsc: true,
  },
});
```

### pledge.config.ts (consumed by PledgePack)
```typescript
import { defineConfig } from 'pledgepack';

export default defineConfig({
  entry: ['app/entry.tsx'],
  framework: 'react',
  appDir: 'app',
  devServer: { port: 3000, hmr: true },
  sourceMaps: true,
  plugins: [
    { name: 'pledgestack-rsc', resolve: '@pledgestack/plugin-rsc' },
    { name: 'pledgestack-ssr', resolve: '@pledgestack/plugin-ssr' },
    { name: 'pledgestack-router', resolve: '@pledgestack/plugin-router' },
  ],
});
```

---

## Dev Server Integration

PledgePack runs the HTTP server. PledgeStack injects middleware:

```
HTTP Request
  → PledgePack dev server (axum)
    → PledgeStack middleware (SSR, RSC, API routes)
      → PledgePack module transform (Oxc)
        → Response (HTML / module / HMR payload)
```

PledgeStack plugins register as PledgePack dev server middleware via the plugin system:

```typescript
// PledgeStack SSR plugin
export default {
  name: 'pledgestack-ssr',
  configureServer(server) {
    app.use(async (req, res, next) => {
      if (req.url.startsWith('/api/')) {
        // Handle API route
        const handler = await resolveApiRoute(req.url);
        return handler(req, res);
      }
      if (req.headers.accept?.includes('text/html')) {
        // SSR render
        const html = await renderToString(req.url);
        return res.html(html);
      }
      next();
    });
  },
};
```

---

## CLI Command Mapping

> **Note (2026-09-15):** PledgePack's npm package used to also register a
> `pledge` bin alias, identical to PledgeStack's own command name — removed
> because the two packages claiming the same global bin name made which one
> actually ran non-deterministic (see `PRODUCTION-READINESS-100.md`).
> PledgePack's commands are `pledgepack dev`/`pledgepack build`/etc. below;
> PledgeStack's own top-level command is still `pledge` (unaffected — it's a
> separate package with its own, non-colliding bin registration). The
> "internally" column was never a literal shell invocation of a `pledge`
> command anyway — `bundler-pledgepack` resolves and spawns the compiled
> `pledgepack` binary directly via `require.resolve('pledgepack/...')`, not
> via a PATH lookup — so this table's *behavior* was never affected by the
> alias; only the labels below are corrected for clarity.

| PledgeStack command | What it does internally |
|-----------------|----------------------|
| `pledge dev` | Spawns the `pledgepack` binary's `dev` command + injects framework middleware |
| `pledge build` | Spawns the `pledgepack` binary's `build` command + runs SSG/SSR post-build |
| `pledge start` | Starts production SSR server (Node.js, not PledgePack) |
| `pledge test` | Spawns the `pledgepack` binary's `test` command (PledgePack handles test runner) |
| `pledge analyze` | Spawns the `pledgepack` binary's `analyze` command (PledgePack handles analyzer) |
| `pledge migrate` | Spawns the `pledgepack` binary's `migrate` command (PledgePack handles migration) |

---

## GitHub Release Binary Naming Convention

PledgePack postinstall expects binaries at:
```
https://github.com/pledgeandgrow/pledgepack/releases/latest/download/pledge-{target}.{ext}
```

| Platform | Target | Extension | Filename |
|----------|--------|-----------|----------|
| Windows x64 | `x86_64-pc-windows-msvc` | `.zip` | `pledge-x86_64-pc-windows-msvc.zip` |
| Windows ARM64 | `aarch64-pc-windows-msvc` | `.zip` | `pledge-aarch64-pc-windows-msvc.zip` |
| macOS arm64 | `aarch64-apple-darwin` | `.tar.gz` | `pledge-aarch64-apple-darwin.tar.gz` |
| macOS x64 | `x86_64-apple-darwin` | `.tar.gz` | `pledge-x86_64-apple-darwin.tar.gz` |
| Linux x64 | `x86_64-unknown-linux-gnu` | `.tar.gz` | `pledge-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `.tar.gz` | `pledge-aarch64-unknown-linux-gnu.tar.gz` |

Inside each archive: a single binary named `pledge` (Unix) or `pledge.exe` (Windows).

---

## Key Files Reference

### PledgePack (pledgepack repo)
- `crates/cli/src/main.rs` — CLI entry point, all commands
- `crates/core/src/config.rs` — PledgeConfig struct, all config fields
- `crates/core/src/config_validate.rs` — Config validation with "Did you mean?" suggestions
- `crates/core/src/pipeline.rs` — Build pipeline (parse → transform → optimize → emit)
- `crates/core/src/transform.rs` — Oxc-based JS/TS/JSX transform
- `crates/core/src/module_graph.rs` — Module dependency graph
- `crates/core/src/router.rs` — File-based routing scanner (`scan_app_dir`)
- `crates/core/src/plugin_system.rs` — Plugin hot reload, lifecycle hooks, parallel execution
- `crates/js-plugin-host/src/lib.rs` — JS plugin host (QuickJS via rquickjs) with Vite-compatible hooks
- `crates/core/src/html.rs` — HTML entry processing
- `crates/core/src/edge.rs` — Edge bundle generation
- `bin/pledge.js` — JS shim that resolves and spawns native binary
- `bin/postinstall.js` — Downloads binary from GitHub Releases
- `package.json` — npm package definition (version `0.4.0`, published on npm as `latest`)

### PledgeStack (pledgeandgrow/pledgestack repo)
- `packages/cli/` — Main framework package (published as `pledgestack` on npm)
- `packages/core/` — Core rendering (SSR, RSC, SSG, static export)
- `packages/server/` — Node.js + edge server runtime, instrumentation, HMR, server utilities
- `packages/shared/` — Shared types and config
- `packages/client/` — Client-side hydration and state
- `packages/auth/` — Authentication middleware
- `packages/state/` — Client state management
- `packages/api/` — API route helpers
- `packages/seo/` — SEO and metadata
- `packages/a11y/` — Accessibility utilities
- `packages/overlay/` — Dev overlay UI

---

## Versioning Strategy

- **PledgePack** and **PledgeStack** version independently
- PledgeStack `package.json` specifies `pledgepack: "^0.4.0"` (caret range, all four package.jsons + `pnpm-workspace.yaml`) — it matches this repo's `0.4.0`, which is published on npm (`latest` dist-tag) and resolves normally
- Breaking changes in PledgePack require PledgeStack to update its dependency range
- PledgeStack can pin PledgePack version for stability: `pledgepack: "0.4.0"` (exact)

---

## Compatibility Matrix

PRODUCTION-READINESS-100.md goal 86. This is the actual mechanism, and the
actual verified-together versions as of this writing — **not** a claim that
every listed PledgePack version has been tested against every listed
PledgeStack version. There is no cross-repo CI yet enforcing this (see the
"Cross-Repo CI" section below); until there is, treat this table as "these
were the versions in each repo's `package.json` on the date shown," not as
independently verified compatibility.

| PledgePack | `bundler-pledgepack` | PledgeStack (`pledgestack` CLI) | Manifest schema version | Date | Verified together? |
|---|---|---|---|---|---|
| 0.4.0 | 0.2.0 | 0.2.0 | 1 (`RouteManifest::SCHEMA_VERSION`) | 2026-09-23 | No — versions recorded from each repo's package.json, not cross-tested. pledgejs's `^0.4.0` range resolves against published `pledgepack@0.4.0`. |
| 0.3.3 | 0.1.4 | 0.1.12 | 1 (`RouteManifest::SCHEMA_VERSION`, added 2026-09-15) | 2026-09-17 | No — versions recorded, not cross-tested. See goal 87. |

**How compatibility is actually checked at runtime** (goal 81, implemented
2026-09-15): every PledgePack dev-server response carries an
`X-Pledgepack-Schema-Version` header, and `__pledge_ps_manifest.json` carries
a `schema_version` field, both sourced from the single
`pledgepack_core::PLEDGESTACK_MANIFEST_SCHEMA_VERSION` constant. `bundler-pledgepack`
checks both (`waitForServer` for the header, `loadManifest` for the manifest
field — see `packages/bundler-pledgepack/src/index.ts`) and **warns**, rather
than hard-fails, on a mismatch — an older PledgeStack talking to a newer
PledgePack (or vice versa) keeps working as long as the fields it actually
reads are still present; the warning is what tells you it might not be safe
to assume so.

To update this table: bump the row whenever you deliberately test a new
PledgePack/PledgeStack pairing together (a real dev server run, not just
"the versions happened to both be installed"), and bump
`RouteManifest::SCHEMA_VERSION` / `PLEDGESTACK_MANIFEST_SCHEMA_VERSION`
together (they're the same constant, re-exported) whenever `RouteManifest`'s
shape changes in a way a consumer should know about.

---

## Integration Failure Modes

PRODUCTION-READINESS-100.md goal 88. The exact, current user-facing
behavior for each way this integration can fail — so failures are
predictable and testable, not "whatever the stack trace happens to say."
Message text below is quoted from the actual source as of 2026-09-15; if
you change the code, keep this table in sync (nothing enforces that
automatically — see goal 92's per-crate-test-count check for the closest
existing analogue, which doesn't cover this specific kind of drift).

| Failure | Where it's detected | User-facing message | Recovery |
|---|---|---|---|
| PledgePack binary not found/not built | `startDevServer`, before spawning | `PledgePack binary not found. Run "cargo build --release" in the pledgepack package.` | Build the binary, or reinstall the `pledgepack` npm package. |
| `runPledgepack()` binary not found (build/transform path) | `binary-resolver.ts`'s `runPledgepack` | "pledgepack binary not found. The native binary may not have been downloaded. Try running 'pnpm rebuild pledgepack' or install the platform-specific package." | Same as above. |
| Dev server port already in use | `checkPortAvailable`, before spawning (goal 80) | `Port {port} on {hostname} is already in use — is another dev server (or a previous PledgePack instance that didn't shut down cleanly) still running on it?` | Stop the other process, or pass a different `bundlerPort`. |
| Spawn itself fails (binary exists but can't exec — wrong arch, corrupted download, permissions) | `proc.on('error', ...)` during startup (goal 78) | `Failed to launch the PledgePack dev server binary ({path}): {os error}` | Re-download/rebuild the binary; check it's executable and matches the host architecture. |
| Process exits before ever responding | `proc.on('exit', ...)` during startup (goal 78) | `PledgePack dev server process exited before it started responding (code={code}, signal={signal}). Check its output above for the real cause.` | The binary's own stdout/stderr (inherited to the parent process) has the real error — read it. |
| Dev server never becomes reachable within 5s, no process-exit signal (hung, or genuinely slow to bind) | `waitForServer` timeout (goal 79) | `PledgePack dev server did not start within 5000ms` + a reason suffix when the retry loop has a consistent error (`ECONNREFUSED`/`ECONNRESET` explained inline) | Check the binary's own output; a persistent `ECONNREFUSED` suffix means the process is alive but never opened the port. |
| Dev server crashes *after* successfully starting | `attachCrashHandler`'s `exit` listener (goal 83) | Logged, not thrown: `[pledgepack] dev server crashed (code=..., signal=...) — restarting in {delay}ms (attempt N/5)`, then after 5 attempts: `...and has exceeded 5 restart attempts — giving up. Run "pledge dev" directly to see the underlying error.` | Automatic up to 5 attempts (exponential backoff, capped at 30s); after that, run `pledge dev` directly to see what's actually failing. |
| `stop()` called but the process doesn't exit from SIGTERM | `stopProcessGracefully` (goal 84) | Logged: `[pledgepack] dev server did not exit within 5000ms of SIGTERM — sending SIGKILL` | Automatic — escalates to SIGKILL. |
| Route manifest missing entirely | `loadManifest` | Silent — `resolveProductionPath` falls through to its other path-guessing strategies. This is intentional (the manifest not existing yet, e.g. before the first build, is a normal state, not an error). | Run `pledge build`. |
| Route manifest present but malformed/unparseable | `loadManifest` (goal 82) | `[pledgepack] Route manifest at {path} is not valid JSON ({error}) — falling back to path-guessing. This usually means the manifest was read mid-write; if it persists, re-run "pledge build".` | Re-run the build; if it persists, the manifest generation itself may be broken (check the PledgePack binary's own output). |
| Route manifest schema version mismatch | `loadManifest` / dev-server response header (goal 81/82) | `[pledgepack] Route manifest at {path} has schema_version {N}, {older,newer} than this adapter {expects,understands} ({M})...` | Upgrade whichever side is behind. Not fatal — proceeds with a warning either way. |
| Production module genuinely not found after a real build | `resolveProductionPath`, all 4 strategies exhausted | `Production module not found: {sourcePath}\nExpected bundled output at: {directPath}\nTried alternatives: ...\nDid you run "pledge build" first?` | Run `pledge build`; if it still fails, the route/file may not exist or the build itself failed. |
| Rust addon (`.psx`/`.ps`) compilation fails | `compileRustAddon` in `packages/server/src/transform.ts` | Mapped source-location errors when possible (goal #210's source-map mapping); otherwise `cargo exited with code {N}` | Read the (source-mapped, where possible) Rust compiler error. |
| Rust addon compilation times out | `compileRustAddon`, `cargo`'s spawn `timeout` (goal 85) | `cargo build timed out after {N}ms` (previously the unhelpful `cargo exited with null`) | Increase `cargoConfig.timeout`, or investigate why the build is slow (cold cache, huge dependency tree). |

---

## Cross-Repo CI

PRODUCTION-READINESS-100.md goal 87 — **not implemented.** Each repo's CI
(`pledgepack/.github/workflows/ci.yml`, and pledgejs's own CI) tests itself
in isolation; nothing runs PledgeStack's dev/build flow against a freshly
built PledgePack binary (or vice versa) before either side ships. This is
real, open work — the compatibility matrix above and the schema-version
checks (goal 81) are the runtime safety net for a mismatch that already
shipped; they're not a substitute for catching one before it ships. A
reasonable shape for this (not yet built): a workflow in one repo that
checks out the other at a pinned ref, builds it, and runs a real
`pledge dev` + a PledgeStack page request against it — attempted; blocked
on this pass by the same repo-in-flux constraints noted elsewhere in
PRODUCTION-READINESS-100.md (Phase 3-4's verification notes).

---

## Publishing Flow

```
1. Build PledgePack binary:     cargo build --release
2. Create GitHub Release:       gh release create v0.4.0 pledge-x86_64-pc-windows-msvc.zip
3. Publish PledgePack to npm:   npm publish (from pledgepack repo)
4. Update PledgeStack dependency:  pledgestack package.json → pledgepack: "^0.4.0" (done — all four package.jsons + pnpm-workspace.yaml; resolves against published 0.4.0)
5. Publish PledgeStack to npm:     npm publish (from packages/cli directory)
```

---

## What PledgeStack Agent Needs to Know

1. **PledgePack is the bundler** — don't reimplement bundling, transforming, or dev server in PledgeStack
2. **PledgeStack wraps PledgePack** — spawn `pledge` binary via `runPledgepack()` from `pledgepack` package
3. **Use PledgePack plugins** — framework features (RSC, SSR, routing) use PledgePack's JS plugin hooks
4. **Virtual modules** — use `resolveId` + `load` plugin hooks for `__pledge_router`, `__pledge_manifest`, etc.
5. **Config** — PledgeStack reads `pledge.config.ts` directly (no separate framework config)
6. **No Rust needed** — PledgeStack is pure TypeScript/JavaScript
7. **Binary is automatic** — `npm install pledgepack` handles binary download via postinstall
8. **Dev server** — PledgePack runs the HTTP server, PledgeStack injects middleware via `configureServer` hook
9. **SSR server** — PledgeStack runs its own Node.js server for production SSR (`pledge start` via `startNodeServer`)
10. **Edge server** — PledgeStack provides `createEdgeHandler` for Cloudflare/Vercel/Deno
11. **Test runner** — use `pledge test` directly, PledgePack has full Vitest-compatible runner
