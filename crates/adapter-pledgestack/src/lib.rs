// PledgeStack adapter: React frontend + Rust backend
//
// .psx/manifest contract — this adapter (pledgepack, the bundler) owns:
//   - Route discovery (app/, server/, middleware)
//   - Route manifest generation (__pledge_ps_manifest.json)
//   - .psx/.ps -> .rs copy for cargo build compatibility
// The PledgeStack framework layer (pledgejs/packages/bundler-pledgepack) owns:
//   - .psx -> TSX transpilation (JSX extraction via pledgestack-core's transformPSX)
//   - Rust addon compilation (spawning cargo for the .node addon)
//   - Consuming the manifest for production path resolution
// See docs/CONNECTION.md for the full responsibility split.
//
// Handles:
//   - app/ directory routing (React .tsx/.psx pages, like Next.js App Router)
//   - app/api/*/route.ts for API route handlers (PledgeStack convention)
//   - server/ directory for Rust backend (.rs or .psx files)
//   - Middleware from root middleware.ts or app/middleware.ts
//   - Route manifest generation
//   - .psx -> .rs copy for cargo build compatibility
//   - Frontend SSR/SSG detection
//   - Route groups (group), optional catch-all [[...slug]], parallel routes @slot
//
// File conventions (PledgeStack):
//   app/page.tsx              -> GET /          (React page)
//   app/about/page.tsx        -> GET /about     (React page)
//   app/blog/[slug]/page.tsx  -> GET /blog/:slug (dynamic route)
//   app/blog/[...slug]/page.tsx -> GET /blog/*slug (catch-all)
//   app/blog/[[...slug]]/page.tsx -> GET /blog/*slug (optional catch-all)
//   app/(marketing)/page.tsx  -> GET /          (route group — skipped from URL)
//   app/@analytics/page.tsx   -> parallel route (skipped from URL)
//   app/layout.tsx            -> shared layout wrapper
//   app/template.tsx          -> like layout but re-renders on navigation
//   app/loading.tsx           -> loading UI for route segment
//   app/error.tsx             -> error boundary for route segment
//   app/global-error.tsx      -> root-level error boundary
//   app/not-found.tsx         -> 404 page
//   app/head.tsx              -> per-segment head metadata
//   app/api/hello/route.ts    -> /api/hello     (API route handler)
//   app/middleware.ts         -> middleware (applies to all routes)
//   middleware.ts             -> root middleware (PledgeStack convention)
//   server/api/users.rs       -> /api/users     (Rust API handler)
//   server/api/users.psx      -> /api/users     (PledgeStack API handler)
//   server/lib.rs             -> server entry point

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// PledgeStack adapter configuration
pub struct PledgeStackAdapter {
    /// Project root
    pub root: PathBuf,
    /// Detected frontend routes (from app/)
    pub frontend_routes: Vec<FrontendRoute>,
    /// Detected API routes (from app/api/*/route.ts or server/api/*.rs)
    pub api_routes: Vec<ApiRoute>,
    /// Detected backend routes (from server/ — Rust .rs/.psx)
    pub backend_routes: Vec<BackendRoute>,
    /// Detected middleware
    pub middleware: Vec<MiddlewareEntry>,
    /// Server entry point file (if any)
    pub server_entry: Option<PathBuf>,
    /// Root layout file (app/layout.tsx)
    pub root_layout: Option<String>,
    /// Global not-found page (app/not-found.tsx)
    pub not_found: Option<String>,
    /// Global error boundary (app/global-error.tsx)
    pub global_error: Option<String>,
}

/// A frontend route (React page)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrontendRoute {
    /// URL path (e.g., "/", "/about", "/blog/:slug")
    pub path: String,
    /// File path relative to project root
    pub file: String,
    /// Dynamic segments (e.g., ["slug"] for /blog/[slug])
    pub params: Vec<String>,
    /// Whether this page supports SSR
    pub ssr: bool,
    /// Whether this page is static (SSG)
    pub static_gen: bool,
    /// Layout file for this route (nearest parent layout)
    pub layout: Option<String>,
    /// Template file for this route (re-renders on navigation)
    pub template: Option<String>,
    /// Loading component for this route
    pub loading: Option<String>,
    /// Error boundary for this route
    pub error_boundary: Option<String>,
    /// Head component for this route
    pub head: Option<String>,
    /// Route type (page or api)
    pub route_type: FrontendRouteType,
}

/// Type of frontend route
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum FrontendRouteType {
    /// React page component (page.tsx)
    Page,
    /// API route handler (route.ts/route.tsx)
    ApiRoute,
}

/// An API route handler (from app/api/*/route.ts)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiRoute {
    /// URL path (e.g., "/api/hello", "/api/users/:id")
    pub path: String,
    /// File path relative to project root
    pub file: String,
    /// Dynamic segments (e.g., ["id"] for /api/users/[id])
    pub params: Vec<String>,
    /// HTTP methods exported (GET, POST, etc.)
    pub methods: Vec<String>,
}

/// A backend route (Rust API handler)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendRoute {
    /// HTTP method (GET, POST, PUT, DELETE, PATCH)
    pub method: String,
    /// URL path (e.g., "/api/users", "/api/users/:id")
    pub path: String,
    /// File path relative to project root
    pub file: String,
    /// Function name in the Rust source
    pub handler: String,
    /// File extension type
    pub ext: BackendExt,
    /// Dynamic segments (e.g., ["id"] for /api/users/[id])
    pub params: Vec<String>,
}

/// Middleware entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiddlewareEntry {
    /// File path relative to project root
    pub file: String,
    /// Middleware name (derived from filename)
    pub name: String,
    /// File extension type
    pub ext: BackendExt,
    /// Routes this middleware applies to (empty = all)
    pub applies_to: Vec<String>,
}

/// Supported backend file extensions
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendExt {
    /// Standard Rust file
    Rs,
    /// PledgeStack PSX file — Rust + JSX hybrid (.psx)
    Psx,
    /// PledgeStack PS file — pure Rust server module (.ps)
    Ps,
}

impl BackendExt {
    /// All supported extensions
    pub const ALL: &[&str] = &["rs", "psx", "ps"];

    /// Parse from file extension
    pub fn from_ext(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "rs" => Some(Self::Rs),
            "psx" => Some(Self::Psx),
            "ps" => Some(Self::Ps),
            _ => None,
        }
    }
}

/// Route manifest generated by the adapter.
///
/// `schema_version` (PRODUCTION-READINESS-100.md goal 81) lets a consumer
/// (PledgeStack's `bundler-pledgepack` adapter, via `loadManifest` in
/// `packages/bundler-pledgepack/src/index.ts`) detect a shape mismatch
/// explicitly instead of a manifest field silently being `undefined` after
/// an incompatible PledgePack upgrade. Bump `RouteManifest::SCHEMA_VERSION`
/// whenever a field is added, removed, or changes meaning — additive
/// changes (a new optional field) don't strictly require a bump, but bump
/// anyway when in doubt; consumers should treat an unrecognized *newer*
/// version as "unknown, proceed cautiously" rather than reject outright, so
/// old PledgeStack versions keep working against a newer schema for as long
/// as they can.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteManifest {
    /// Manifest schema version — see [`RouteManifest::SCHEMA_VERSION`].
    #[serde(default)]
    pub schema_version: u32,
    /// Frontend routes (pages)
    pub frontend: Vec<FrontendRoute>,
    /// API routes (from app/api/*/route.ts)
    pub api: Vec<ApiRoute>,
    /// Backend routes (from server/ — Rust)
    pub backend: Vec<BackendRoute>,
    /// Middleware
    pub middleware: Vec<MiddlewareEntry>,
    /// Server entry point (relative path, if any)
    pub server_entry: Option<String>,
    /// Root layout file
    pub root_layout: Option<String>,
    /// Global not-found page
    pub not_found: Option<String>,
    /// Global error boundary
    pub global_error: Option<String>,
}

impl RouteManifest {
    /// Current manifest schema version. Bump when `RouteManifest`'s shape
    /// changes in a way a consumer should know about. See the struct's doc
    /// comment. Re-exports `pledgepack_core::PLEDGESTACK_MANIFEST_SCHEMA_VERSION`
    /// — the shared constant `pledgepack-dev-server` also stamps onto its
    /// `/__pledge_router` response header, so both surfaces agree by
    /// construction rather than by two constants staying in sync manually.
    pub const SCHEMA_VERSION: u32 = pledgepack_core::PLEDGESTACK_MANIFEST_SCHEMA_VERSION;
}

impl PledgeStackAdapter {
    /// Create a new adapter for the given project root
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            frontend_routes: Vec::new(),
            api_routes: Vec::new(),
            backend_routes: Vec::new(),
            middleware: Vec::new(),
            server_entry: None,
            root_layout: None,
            not_found: None,
            global_error: None,
        }
    }

    /// Discover all routes -- frontend (app/), API routes, backend (server/), middleware
    pub fn discover(&mut self) -> Result<()> {
        self.discover_frontend_routes()?;
        self.discover_api_routes()?;
        self.discover_backend_routes()?;
        self.discover_middleware()?;
        self.discover_server_entry()?;
        Ok(())
    }

    /// Scan app/ directory for React frontend routes
    fn discover_frontend_routes(&mut self) -> Result<()> {
        let app_dir = self.root.join("app");
        if !app_dir.exists() || !app_dir.is_dir() {
            return Ok(());
        }

        // Detect root-level files
        self.root_layout = find_convention_file(
            &app_dir,
            &[
                "layout.tsx",
                "layout.ts",
                "layout.jsx",
                "layout.js",
                "layout.psx",
                "layout.ps",
            ],
        )
        .map(|p| relative_path(&self.root, &p));
        self.not_found = find_convention_file(
            &app_dir,
            &[
                "not-found.tsx",
                "not-found.ts",
                "not-found.jsx",
                "not-found.js",
                "not-found.psx",
                "not-found.ps",
            ],
        )
        .map(|p| relative_path(&self.root, &p));
        self.global_error = find_convention_file(
            &app_dir,
            &[
                "global-error.tsx",
                "global-error.ts",
                "global-error.jsx",
                "global-error.js",
                "global-error.psx",
                "global-error.ps",
            ],
        )
        .map(|p| relative_path(&self.root, &p));

        self.scan_app_directory(&app_dir, &app_dir, "")?;
        Ok(())
    }

    /// Recursively scan app/ directory for page.tsx and route.ts files
    fn scan_app_directory(&mut self, app_dir: &Path, dir: &Path, prefix: &str) -> Result<()> {
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("Failed to read app directory: {}", dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // Skip private folders (starting with _ or .), node_modules,
                // and symlinked directories. The symlink check specifically
                // (PRODUCTION-READINESS-100.md goal 30) matters because
                // `prepare_psx_files` later copies whatever files this scan
                // finds using only a lexical (non-canonicalizing) path check
                // — a directory symlink followed here could walk this scan
                // outside `app_dir` entirely, and a symlink cycle could hang
                // the scan. `entry.file_type()` reports the symlink itself
                // (unlike `path.is_dir()`, which follows it), so this check
                // has to use that, not `path.is_dir()`.
                if name.starts_with('_')
                    || name.starts_with('.')
                    || name == "node_modules"
                    || entry.file_type().map(|t| t.is_symlink()).unwrap_or(false)
                {
                    continue;
                }

                // Route groups (group) — skipped from URL, but still scanned
                if name.starts_with('(') && name.ends_with(')') {
                    self.scan_app_directory(app_dir, &path, prefix)?;
                    continue;
                }

                // Parallel routes @slot — skipped from URL, but still scanned
                if name.starts_with('@') {
                    self.scan_app_directory(app_dir, &path, prefix)?;
                    continue;
                }

                let route_segment = parse_route_segment(&name);
                let new_prefix = if prefix.is_empty() {
                    format!("/{}", route_segment)
                } else {
                    format!("{}/{}", prefix, route_segment)
                };
                self.scan_app_directory(app_dir, &path, &new_prefix)?;
            } else if path.is_file() {
                let is_page = matches!(
                    name.as_str(),
                    "page.tsx" | "page.ts" | "page.jsx" | "page.js" | "page.psx" | "page.ps"
                );
                let is_route = matches!(
                    name.as_str(),
                    "route.tsx" | "route.ts" | "route.jsx" | "route.js" | "route.psx" | "route.ps"
                );

                if is_page {
                    let route_path = if prefix.is_empty() {
                        "/".to_string()
                    } else {
                        prefix.to_string()
                    };
                    let params = extract_params(&route_path);
                    let rel_file = relative_path(&self.root, &path);
                    let (ssr, static_gen) = detect_render_mode(&path);

                    // Find layout, template, loading, error, head for this route
                    let route_dir = path.parent().unwrap_or(app_dir);
                    let layout = self.find_nearest_layout(app_dir, route_dir);
                    let template = find_convention_file(
                        route_dir,
                        &[
                            "template.tsx",
                            "template.ts",
                            "template.jsx",
                            "template.js",
                            "template.psx",
                            "template.ps",
                        ],
                    )
                    .map(|p| relative_path(&self.root, &p));
                    let loading = find_convention_file(
                        route_dir,
                        &[
                            "loading.tsx",
                            "loading.ts",
                            "loading.jsx",
                            "loading.js",
                            "loading.psx",
                            "loading.ps",
                        ],
                    )
                    .map(|p| relative_path(&self.root, &p));
                    let error_boundary = find_convention_file(
                        route_dir,
                        &[
                            "error.tsx",
                            "error.ts",
                            "error.jsx",
                            "error.js",
                            "error.psx",
                            "error.ps",
                        ],
                    )
                    .map(|p| relative_path(&self.root, &p));
                    let head = find_convention_file(
                        route_dir,
                        &[
                            "head.tsx", "head.ts", "head.jsx", "head.js", "head.psx", "head.ps",
                        ],
                    )
                    .map(|p| relative_path(&self.root, &p));

                    self.frontend_routes.push(FrontendRoute {
                        path: route_path,
                        file: rel_file,
                        params,
                        ssr,
                        static_gen,
                        layout,
                        template,
                        loading,
                        error_boundary,
                        head,
                        route_type: FrontendRouteType::Page,
                    });
                } else if is_route {
                    // Route handler files are discovered separately in discover_api_routes
                    // but we also handle them here for app/ routes outside api/
                    let route_path = if prefix.is_empty() {
                        "/".to_string()
                    } else {
                        prefix.to_string()
                    };
                    let params = extract_params(&route_path);
                    let rel_file = relative_path(&self.root, &path);
                    let methods = detect_route_methods(&path);

                    self.api_routes.push(ApiRoute {
                        path: route_path,
                        file: rel_file,
                        params,
                        methods,
                    });
                }
            }
        }
        Ok(())
    }

    /// Find the nearest layout file by walking up from route_dir to app_dir
    fn find_nearest_layout(&self, app_dir: &Path, route_dir: &Path) -> Option<String> {
        let mut current = route_dir;
        loop {
            if let Some(layout_path) = find_convention_file(
                current,
                &[
                    "layout.tsx",
                    "layout.ts",
                    "layout.jsx",
                    "layout.js",
                    "layout.psx",
                    "layout.ps",
                ],
            ) {
                return Some(relative_path(&self.root, &layout_path));
            }
            if current == app_dir {
                break;
            }
            current = current.parent().unwrap_or(app_dir);
        }
        None
    }

    /// Scan app/ directory for API route handlers (route.ts/route.tsx/route.psx)
    fn discover_api_routes(&mut self) -> Result<()> {
        let app_dir = self.root.join("app");
        if !app_dir.exists() || !app_dir.is_dir() {
            return Ok(());
        }
        self.scan_api_routes_in_app(&app_dir, &app_dir, "")?;
        Ok(())
    }

    /// Recursively scan app/ for route.ts files (API handlers)
    #[allow(clippy::only_used_in_recursion)]
    fn scan_api_routes_in_app(&mut self, app_dir: &Path, dir: &Path, prefix: &str) -> Result<()> {
        let entries = std::fs::read_dir(dir).with_context(|| {
            format!(
                "Failed to read app directory for API routes: {}",
                dir.display()
            )
        })?;

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // See the matching comment in `scan_app_directory` above.
                if name.starts_with('_')
                    || name.starts_with('.')
                    || name == "node_modules"
                    || entry.file_type().map(|t| t.is_symlink()).unwrap_or(false)
                {
                    continue;
                }

                // Route groups and parallel routes — skipped from URL
                if name.starts_with('(') && name.ends_with(')') {
                    self.scan_api_routes_in_app(app_dir, &path, prefix)?;
                    continue;
                }
                if name.starts_with('@') {
                    self.scan_api_routes_in_app(app_dir, &path, prefix)?;
                    continue;
                }

                let route_segment = parse_route_segment(&name);
                let new_prefix = if prefix.is_empty() {
                    format!("/{}", route_segment)
                } else {
                    format!("{}/{}", prefix, route_segment)
                };
                self.scan_api_routes_in_app(app_dir, &path, &new_prefix)?;
            } else if path.is_file()
                && matches!(
                    name.as_str(),
                    "route.tsx" | "route.ts" | "route.jsx" | "route.js" | "route.psx" | "route.ps"
                )
            {
                let route_path = if prefix.is_empty() {
                    "/".to_string()
                } else {
                    prefix.to_string()
                };
                let params = extract_params(&route_path);
                let rel_file = relative_path(&self.root, &path);
                let methods = detect_route_methods(&path);

                // Only add if not already discovered by scan_app_directory
                if !self.api_routes.iter().any(|r| r.file == rel_file) {
                    self.api_routes.push(ApiRoute {
                        path: route_path,
                        file: rel_file,
                        params,
                        methods,
                    });
                }
            }
        }
        Ok(())
    }

    /// Scan server/ directory for Rust backend routes (.rs and .psx)
    fn discover_backend_routes(&mut self) -> Result<()> {
        let server_api_dir = self.root.join("server").join("api");
        if !server_api_dir.exists() || !server_api_dir.is_dir() {
            return Ok(());
        }
        self.scan_api_directory(&server_api_dir, "/api")?;
        Ok(())
    }

    /// Recursively scan server/api/ for .rs and .psx route files
    fn scan_api_directory(&mut self, dir: &Path, prefix: &str) -> Result<()> {
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("Failed to read server/api directory: {}", dir.display()))?;

        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // See the matching comment in `scan_app_directory` above —
                // same reasoning applies here, and this scan is the more
                // directly security-relevant of the two: its output feeds
                // `prepare_psx_files`'s file copies.
                if name.starts_with('_')
                    || name.starts_with('.')
                    || name == "node_modules"
                    || entry.file_type().map(|t| t.is_symlink()).unwrap_or(false)
                {
                    continue;
                }
                let route_segment = parse_route_segment(&name);
                let new_prefix = format!("{}/{}", prefix, route_segment);
                self.scan_api_directory(&path, &new_prefix)?;
            } else if path.is_file() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if let Some(backend_ext) = BackendExt::from_ext(ext) {
                    let routes = extract_backend_routes(&path, prefix, backend_ext, &self.root)?;
                    self.backend_routes.extend(routes);
                }
            }
        }
        Ok(())
    }

    /// Scan for middleware files (root middleware.ts, app/middleware.ts, or server/middleware/)
    fn discover_middleware(&mut self) -> Result<()> {
        // Check root-level middleware.ts (PledgeStack convention)
        let root_mw_candidates = [
            "middleware.ts",
            "middleware.tsx",
            "middleware.js",
            "middleware.jsx",
            "middleware.psx",
            "middleware.ps",
        ];
        for candidate in &root_mw_candidates {
            let path = self.root.join(candidate);
            if path.exists() {
                let rel_file = relative_path(&self.root, &path);
                let mw_name = "root".to_string();
                self.middleware.push(MiddlewareEntry {
                    file: rel_file,
                    name: mw_name,
                    ext: BackendExt::Psx, // .ts/.tsx middleware is JS-based
                    applies_to: Vec::new(),
                });
                break;
            }
        }

        // Check app/middleware.ts (PledgeStack convention)
        let app_mw_candidates = [
            "middleware.ts",
            "middleware.tsx",
            "middleware.js",
            "middleware.jsx",
            "middleware.psx",
            "middleware.ps",
        ];
        for candidate in &app_mw_candidates {
            let path = self.root.join("app").join(candidate);
            if path.exists() {
                let rel_file = relative_path(&self.root, &path);
                let mw_name = "app".to_string();
                // Avoid duplicates
                if !self.middleware.iter().any(|m| m.file == rel_file) {
                    self.middleware.push(MiddlewareEntry {
                        file: rel_file,
                        name: mw_name,
                        ext: BackendExt::Psx,
                        applies_to: Vec::new(),
                    });
                }
                break;
            }
        }

        // Check server/middleware/ directory (Rust backend middleware)
        let mw_dir = self.root.join("server").join("middleware");
        if mw_dir.exists() && mw_dir.is_dir() {
            let entries = std::fs::read_dir(mw_dir)?;
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if !path.is_file() {
                    continue;
                }
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if let Some(backend_ext) = BackendExt::from_ext(ext) {
                    let mw_name = name.split('.').next().unwrap_or("middleware").to_string();
                    let rel_file = relative_path(&self.root, &path);
                    self.middleware.push(MiddlewareEntry {
                        file: rel_file,
                        name: mw_name,
                        ext: backend_ext,
                        applies_to: Vec::new(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Find server entry point (server/lib.rs or server/lib.psx or server/lib.ps or server/main.rs)
    fn discover_server_entry(&mut self) -> Result<()> {
        let server_dir = self.root.join("server");
        if !server_dir.exists() || !server_dir.is_dir() {
            return Ok(());
        }
        for candidate in &[
            "lib.rs", "lib.psx", "lib.ps", "main.rs", "main.psx", "main.ps",
        ] {
            let path = server_dir.join(candidate);
            if path.exists() {
                self.server_entry = Some(path);
                break;
            }
        }
        Ok(())
    }

    /// Generate a route manifest
    pub fn manifest(&self) -> RouteManifest {
        RouteManifest {
            schema_version: RouteManifest::SCHEMA_VERSION,
            frontend: self.frontend_routes.clone(),
            api: self.api_routes.clone(),
            backend: self.backend_routes.clone(),
            middleware: self.middleware.clone(),
            server_entry: self.server_entry.as_ref().and_then(|p| {
                p.strip_prefix(&self.root)
                    .ok()
                    .map(|r| pledgepack_core::normalize_path(r))
            }),
            root_layout: self.root_layout.clone(),
            not_found: self.not_found.clone(),
            global_error: self.global_error.clone(),
        }
    }

    /// Copy .psx and .ps files to .rs in the output directory for cargo build
    pub fn prepare_psx_files(&self, out_dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
        let mut copied = Vec::new();

        for route in &self.backend_routes {
            if route.ext == BackendExt::Psx || route.ext == BackendExt::Ps {
                let rel = route
                    .file
                    .strip_suffix(".psx")
                    .or_else(|| route.file.strip_suffix(".ps"))
                    .unwrap_or(&route.file);
                let dst_rel = format!("{}.rs", rel);
                if let Some(pair) =
                    self.copy_psx_file(&route.file, &dst_rel, out_dir, "backend route")?
                {
                    copied.push(pair);
                }
            }
        }

        for mw in &self.middleware {
            if mw.ext == BackendExt::Psx || mw.ext == BackendExt::Ps {
                let rel = mw
                    .file
                    .strip_suffix(".psx")
                    .or_else(|| mw.file.strip_suffix(".ps"))
                    .unwrap_or(&mw.file);
                let dst_rel = format!("{}.rs", rel);
                if let Some(pair) = self.copy_psx_file(&mw.file, &dst_rel, out_dir, "middleware")? {
                    copied.push(pair);
                }
            }
        }

        if let Some(ref entry) = self.server_entry {
            let ext = entry.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "psx" || ext == "ps" {
                let rel = pledgepack_core::normalize_path(
                    entry.strip_prefix(&self.root).unwrap_or(entry),
                );
                let rel_no_ext = rel
                    .strip_suffix(".psx")
                    .or_else(|| rel.strip_suffix(".ps"))
                    .unwrap_or(&rel);
                let dst_rel = format!("{}.rs", rel_no_ext);
                if let Some(pair) = self.copy_psx_file(&rel, &dst_rel, out_dir, "server entry")? {
                    copied.push(pair);
                }
            }
        }

        Ok(copied)
    }

    /// Shared validate-and-copy logic for `prepare_psx_files`'s three call
    /// sites (backend routes, middleware, server entry). Beyond the existing
    /// lexical `validate_path_within` traversal check on both the source and
    /// destination path *strings*, this also refuses to copy a source that
    /// is itself a symlink: `validate_path_within` normalizes the path
    /// textually, so a path string like `server/api/x.psx` passes even when
    /// that entry is actually a symlink pointing outside `self.root` —
    /// `std::fs::copy` follows symlinks, so without this check the file
    /// *content* copied into the build output could come from anywhere the
    /// symlink points, not from inside the project. See
    /// PRODUCTION-READINESS-100.md goal 30.
    fn copy_psx_file(
        &self,
        src_rel: &str,
        dst_rel: &str,
        out_dir: &Path,
        kind: &str,
    ) -> Result<Option<(PathBuf, PathBuf)>> {
        let (src, dst) = match (
            validate_path_within(&self.root, src_rel),
            validate_path_within(out_dir, dst_rel),
        ) {
            (Some(s), Some(d)) => (s, d),
            _ => {
                eprintln!("Warning: skipping {kind} with path traversal: {src_rel}");
                return Ok(None);
            }
        };

        match std::fs::symlink_metadata(&src) {
            Ok(meta) if meta.file_type().is_symlink() => {
                eprintln!(
                    "Warning: skipping {kind} because it is a symlink (not following it to \
                     avoid copying content from outside the project): {src_rel}"
                );
                return Ok(None);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Warning: skipping {kind} — could not stat {src_rel}: {e}");
                return Ok(None);
            }
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst)?;
        Ok(Some((src, dst)))
    }

    /// Write the route manifest to a JSON file
    pub fn write_manifest(&self, out_path: &Path) -> Result<()> {
        let manifest = self.manifest();
        let json = serde_json::to_string_pretty(&manifest)?;
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(out_path, json)?;
        Ok(())
    }
}

/// Extract dynamic params from a route path (e.g., "/blog/:slug" -> ["slug"])
fn extract_params(path: &str) -> Vec<String> {
    path.split('/')
        .filter_map(|seg| {
            seg.strip_prefix(':')
                .or_else(|| seg.strip_prefix('*'))
                .map(|rest| rest.to_string())
        })
        .collect()
}

/// Parse a route segment from a directory name, handling dynamic segments,
/// catch-all, optional catch-all, route groups, and parallel routes.
fn parse_route_segment(name: &str) -> String {
    if name.starts_with("[[") && name.ends_with("]]") {
        // Optional catch-all: [[...slug]]
        let inner = &name[2..name.len() - 2];
        if let Some(rest) = inner.strip_prefix("...") {
            return format!("*{}", rest);
        }
        return name.to_string();
    }
    if name.starts_with('[') && name.ends_with(']') {
        let inner = &name[1..name.len() - 1];
        if let Some(rest) = inner.strip_prefix("...") {
            // Catch-all: [...slug]
            return format!("*{}", rest);
        }
        // Dynamic param: [slug]
        return format!(":{}", inner);
    }
    name.to_string()
}

/// Find the first matching file from a list of candidate names in a directory
fn find_convention_file(dir: &Path, candidates: &[&str]) -> Option<PathBuf> {
    for name in candidates {
        let path = dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Get a path relative to the root, with forward slashes
fn relative_path(root: &Path, path: &Path) -> String {
    pledgepack_core::normalize_path(path.strip_prefix(root).unwrap_or(path))
}

/// Validates that a path stays within the base directory after normalization.
///
/// Joins `input` to `base`, normalizes `.` and `..` components, and returns the
/// normalized path only if it remains within `base`. Returns `None` if the
/// normalized path escapes `base` (e.g. via `../` traversal sequences).
fn validate_path_within(base: &Path, input: &str) -> Option<PathBuf> {
    let joined = base.join(input.trim_start_matches('/'));
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut base_normalized = PathBuf::new();
    for component in base.components() {
        match component {
            std::path::Component::ParentDir => {
                base_normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => base_normalized.push(other.as_os_str()),
        }
    }
    if normalized.starts_with(&base_normalized) {
        Some(normalized)
    } else {
        None
    }
}

/// Detect HTTP methods exported by a route handler file
///
/// Uses a line-by-line scan that skips comment lines (single-line `//` and
/// block `/* */` comments) so that method names appearing inside comments or
/// string literals are not mistaken for real exports. Only `export const`,
/// `export function`, and `export async function` declarations at the start of
/// a (trimmed) line are recognized.
fn detect_route_methods(path: &Path) -> Vec<String> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let methods = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
    let mut found = Vec::new();
    let mut in_block_comment = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }

        for method in &methods {
            if trimmed.starts_with(&format!("export const {}", method))
                || trimmed.starts_with(&format!("export async function {}", method))
                || trimmed.starts_with(&format!("export function {}", method))
            {
                if !found.contains(&method.to_string()) {
                    found.push(method.to_string());
                }
            }
        }
    }

    if found.is_empty() {
        found.push("GET".to_string());
    }
    found
}

/// Detect whether a page supports SSR or is static
///
/// Uses a line-by-line scan that skips comment lines (single-line `//` and
/// block `/* */` comments) so that directives appearing inside comments or
/// string literals are not mistaken for real exports. Recognizes
/// `getServerSideProps`, `getStaticProps`, `generateStaticParams`,
/// `export const dynamic`, and `export const revalidate` declarations.
fn detect_render_mode(path: &Path) -> (bool, bool) {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut has_ssr = false;
    let mut has_ssg = false;
    let mut in_block_comment = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if in_block_comment {
            if trimmed.contains("*/") {
                in_block_comment = false;
            }
            continue;
        }
        if trimmed.starts_with("/*") {
            if !trimmed.contains("*/") {
                in_block_comment = true;
            }
            continue;
        }
        if trimmed.starts_with("//") {
            continue;
        }

        if trimmed.contains("getServerSideProps") {
            has_ssr = true;
        }
        if trimmed.contains("getStaticProps") || trimmed.contains("generateStaticParams") {
            has_ssg = true;
        }
        // Match export const dynamic = 'force-dynamic' etc.
        if trimmed.contains("export const dynamic") {
            if trimmed.contains("force-dynamic") {
                has_ssr = true;
            }
            if trimmed.contains("force-static") {
                has_ssg = true;
            }
        }
        if trimmed.contains("export const revalidate") {
            if trimmed.contains("revalidate = 0") || trimmed.contains("revalidate=0") {
                has_ssr = true;
            } else {
                has_ssg = true;
            }
        }
    }

    (has_ssr, has_ssg)
}

/// Extract backend routes from a .rs or .psx file by scanning for #[route(...)] macros
fn extract_backend_routes(
    path: &Path,
    prefix: &str,
    ext: BackendExt,
    root: &Path,
) -> Result<Vec<BackendRoute>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read backend route file: {}", path.display()))?;

    let rel_file = relative_path(root, path);

    let mut routes = Vec::new();
    let mut pending: Option<(String, String)> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if (trimmed.starts_with("#[route(") || trimmed.starts_with("#[pledge::route("))
            && let Some((method, route_path)) = parse_route_attr(trimmed)
        {
            // Combine directory prefix with route path
            let full_path = if route_path.starts_with('/') {
                if prefix.is_empty() {
                    route_path.clone()
                } else {
                    format!("{}{}", prefix, route_path)
                }
            } else {
                format!("{}/{}", prefix, route_path)
            };
            pending = Some((method, full_path));
        }

        if let Some((method, route_path)) = &pending
            && let Some(fn_name) = extract_fn_name(trimmed)
        {
            let params = extract_params(route_path);
            routes.push(BackendRoute {
                method: method.clone(),
                path: route_path.clone(),
                file: rel_file.clone(),
                handler: fn_name,
                ext,
                params,
            });
            pending = None;
        }
    }

    Ok(routes)
}

/// Parse a #[route(...)] attribute line to extract method and path
fn parse_route_attr(line: &str) -> Option<(String, String)> {
    let inner = line
        .strip_prefix("#[route(")
        .or_else(|| line.strip_prefix("#[pledge::route("))
        .and_then(|s| s.strip_suffix(")]"))
        .unwrap_or("");

    // Check for key-value format first: method = "GET", path = "/api/users"
    if inner.contains('=') {
        let mut method = None;
        let mut path = None;
        for part in inner.split(',') {
            let part = part.trim();
            if let Some(val) = part.strip_prefix("method") {
                let val = val.trim();
                if let Some(quoted) = val.strip_prefix("=") {
                    method = Some(quoted.trim().trim_matches('"').to_string());
                }
            } else if let Some(val) = part.strip_prefix("path") {
                let val = val.trim();
                if let Some(quoted) = val.strip_prefix("=") {
                    path = Some(quoted.trim().trim_matches('"').to_string());
                }
            }
        }
        return match (method, path) {
            (Some(m), Some(p)) => Some((m, p)),
            _ => None,
        };
    }

    // Simple format: GET, "/api/users"
    if let Some(comma_pos) = inner.find(',') {
        let method = inner[..comma_pos].trim().trim_matches('"').to_string();
        let path = inner[comma_pos + 1..].trim().trim_matches('"').to_string();
        if !method.is_empty() && !path.is_empty() {
            return Some((method, path));
        }
    }

    None
}

/// Extract function name from a `pub async fn name(` or `pub fn name(` line
fn extract_fn_name(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(fn_pos) = line.find("fn ") {
        let after_fn = &line[fn_pos + 3..];
        if let Some(paren_pos) = after_fn.find('(') {
            let name = after_fn[..paren_pos].trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_ext_from_str() {
        assert_eq!(BackendExt::from_ext("rs"), Some(BackendExt::Rs));
        assert_eq!(BackendExt::from_ext("psx"), Some(BackendExt::Psx));
        assert_eq!(BackendExt::from_ext("ps"), Some(BackendExt::Ps));
        assert_eq!(BackendExt::from_ext("tsx"), None);
    }

    #[test]
    fn test_extract_params() {
        assert_eq!(extract_params("/blog/:slug"), vec!["slug"]);
        assert_eq!(
            extract_params("/api/users/:id/posts/:post_id"),
            vec!["id", "post_id"]
        );
        assert_eq!(extract_params("/"), Vec::<String>::new());
    }

    #[test]
    fn test_parse_route_attr_simple() {
        let result = parse_route_attr(r#"#[route(GET, "/api/users")]"#);
        assert_eq!(result, Some(("GET".to_string(), "/api/users".to_string())));
    }

    #[test]
    fn test_parse_route_attr_qualified() {
        let result = parse_route_attr(r#"#[pledge::route(POST, "/api/auth")]"#);
        assert_eq!(result, Some(("POST".to_string(), "/api/auth".to_string())));
    }

    #[test]
    fn test_parse_route_attr_kv() {
        let result = parse_route_attr(r#"#[route(method = "DELETE", path = "/api/users/:id")]"#);
        assert_eq!(
            result,
            Some(("DELETE".to_string(), "/api/users/:id".to_string()))
        );
    }

    #[test]
    fn test_extract_fn_name() {
        assert_eq!(
            extract_fn_name("pub async fn list_users("),
            Some("list_users".to_string())
        );
        assert_eq!(
            extract_fn_name("pub fn handler("),
            Some("handler".to_string())
        );
        assert_eq!(extract_fn_name("fn not_pub("), Some("not_pub".to_string()));
        assert_eq!(extract_fn_name("not a function"), None);
    }
}
