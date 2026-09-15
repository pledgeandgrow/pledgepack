// Next.js adapter: File-based routing, SSR, API routes
//
// Full App Router (Next.js 13-16) and Pages Router support:
//   - app/ directory routing with all file conventions
//   - pages/ directory routing (Pages Router)
//   - Route handlers (route.ts) — App Router API endpoints
//   - API routes (pages/api/)
//   - Dynamic segments: [param], [...slug], [[...slug]]
//   - Route groups: (group)
//   - Parallel routes: @slot
//   - Intercepting routes: (.), (..), (...)
//   - File conventions: page, layout, loading, error, not-found,
//     template, default, global-error, route
//   - middleware.ts detection
//   - src/ directory support
//   - .mdx page support
//   - Server-side rendering (SSR) / Static site generation (SSG)
//   - Layout nesting
//   - Loading/error boundaries

use anyhow::Result;
use std::path::{Path, PathBuf};

/// Next.js adapter configuration
pub struct NextAdapter {
    /// Project root
    pub root: PathBuf,
    /// Use App Router (app/) or Pages Router (pages/)
    pub router_type: RouterType,
    /// Detected routes
    pub routes: Vec<Route>,
    /// Detected middleware file path (relative to root)
    pub middleware: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterType {
    AppRouter,
    PagesRouter,
}

#[derive(Debug, Clone)]
pub struct Route {
    /// URL path (e.g., "/about", "/posts/:id", "/docs/*slug")
    pub path: String,
    /// File path relative to project root
    pub file: String,
    /// Route type
    pub kind: RouteKind,
    /// Dynamic segments (e.g., ["id"] for /posts/[id])
    pub params: Vec<String>,
    /// Whether this route has a catch-all segment
    pub catch_all: bool,
    /// Whether the catch-all is optional ([[...slug]])
    pub catch_all_optional: bool,
    /// Parallel route slot name (e.g., "@auth") if applicable
    pub slot: Option<String>,
    /// Intercepting route prefix if applicable
    pub intercept: Option<InterceptKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    Page,
    Layout,
    Loading,
    Error,
    Api,
    NotFound,
    Template,
    Default,
    GlobalError,
    RouteHandler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterceptKind {
    /// (.) — same level
    Same,
    /// (..) — one level up
    Parent,
    /// (..)(..) — two levels up
    GrandParent,
    /// (...) — from root
    Root,
}

impl NextAdapter {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            router_type: RouterType::AppRouter,
            routes: Vec::new(),
            middleware: None,
        }
    }

    /// Detect all routes from the file system
    pub fn discover_routes(&mut self) -> Result<()> {
        // Clear any previously discovered routes and middleware before rescanning
        self.routes.clear();
        self.middleware = None;

        // Detect middleware.ts (can be at root or src/)
        self.detect_middleware();

        // Check for app/ directory — supports both root/app and root/src/app
        let app_dir = self.root.join("app");
        let src_app_dir = self.root.join("src").join("app");

        if app_dir.exists() && app_dir.is_dir() {
            self.router_type = RouterType::AppRouter;
            self.discover_app_routes(&app_dir, "", &[], None, None)?;
        } else if src_app_dir.exists() && src_app_dir.is_dir() {
            self.router_type = RouterType::AppRouter;
            self.discover_app_routes(&src_app_dir, "", &[], None, None)?;
        }

        // Check for pages/ directory — supports both root/pages and root/src/pages
        let pages_dir = self.root.join("pages");
        let src_pages_dir = self.root.join("src").join("pages");

        if pages_dir.exists() && pages_dir.is_dir() {
            if self.routes.is_empty() {
                self.router_type = RouterType::PagesRouter;
            }
            self.discover_pages_routes(&pages_dir, "", None, None)?;
        } else if src_pages_dir.exists() && src_pages_dir.is_dir() {
            if self.routes.is_empty() {
                self.router_type = RouterType::PagesRouter;
            }
            self.discover_pages_routes(&src_pages_dir, "", None, None)?;
        }

        Ok(())
    }

    /// Detect middleware.ts/js at project root or src/
    fn detect_middleware(&mut self) {
        for base in [&self.root, &self.root.join("src")] {
            for ext in ["ts", "js", "tsx", "jsx"] {
                let mw = base.join(format!("middleware.{}", ext));
                if mw.exists() {
                    self.middleware = Some(pledgepack_core::normalize_path(
                        mw.strip_prefix(&self.root).unwrap_or(&mw),
                    ));
                    return;
                }
            }
        }
    }

    /// Discover routes from app/ directory (App Router)
    /// `accumulated_params` tracks dynamic params from parent directories
    /// `current_slot` is the parallel route slot name inherited from a parent `@slot` directory
    /// `current_intercept` is the intercepting route kind inherited from a parent intercept directory
    fn discover_app_routes(
        &mut self,
        dir: &Path,
        prefix: &str,
        accumulated_params: &[String],
        current_slot: Option<String>,
        current_intercept: Option<InterceptKind>,
    ) -> Result<()> {
        let entries = std::fs::read_dir(dir)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // Parse directory name for routing convention
                let dir_info = parse_dir_name(&name);

                // Compute slot/intercept for this directory level.
                // A `@slot` directory sets the parallel route slot name.
                // An intercepting route prefix (`(.)`, `(..)`, etc.) sets the intercept kind.
                // Nested directories inherit the parent's slot/intercept unless overridden.
                let new_slot = if name.starts_with('@') {
                    Some(name.trim_start_matches('@').to_string())
                } else {
                    current_slot.clone()
                };
                let new_intercept = parse_intercept(&name).or(current_intercept);

                let new_prefix = match &dir_info.segment {
                    DirSegment::None => prefix.to_string(),
                    DirSegment::Static(seg) => join_path(prefix, seg),
                    DirSegment::Dynamic(param) => join_path(prefix, &format!(":{}", param)),
                    DirSegment::CatchAll(param) => join_path(prefix, &format!("*{}", param)),
                    DirSegment::CatchAllOptional(param) => {
                        join_path(prefix, &format!("*?{}", param))
                    }
                };

                let mut child_params = accumulated_params.to_vec();
                if let Some(p) = &dir_info.param {
                    child_params.push(p.clone());
                }

                // Check for page/route files in this directory
                let page_exts = ["page.tsx", "page.ts", "page.jsx", "page.js", "page.mdx"];
                let route_exts = ["route.ts", "route.js", "route.tsx", "route.jsx"];

                let page_file = find_file(&path, &page_exts);
                let route_file = find_file(&path, &route_exts);

                if page_file.exists() {
                    self.routes.push(Route {
                        path: new_prefix.clone(),
                        file: rel_path(&self.root, &page_file),
                        kind: RouteKind::Page,
                        params: child_params.clone(),
                        catch_all: dir_info.is_catch_all,
                        catch_all_optional: dir_info.is_catch_all_optional,
                        slot: new_slot.clone(),
                        intercept: new_intercept,
                    });
                }

                if route_file.exists() {
                    self.routes.push(Route {
                        path: new_prefix.clone(),
                        file: rel_path(&self.root, &route_file),
                        kind: RouteKind::RouteHandler,
                        params: child_params.clone(),
                        catch_all: dir_info.is_catch_all,
                        catch_all_optional: dir_info.is_catch_all_optional,
                        slot: new_slot.clone(),
                        intercept: new_intercept,
                    });
                }

                // Recurse into subdirectories
                self.discover_app_routes(
                    &path,
                    &new_prefix,
                    &child_params,
                    new_slot,
                    new_intercept,
                )?;
            } else {
                // File-level route conventions
                let kind = match name.as_str() {
                    "page.tsx" | "page.ts" | "page.jsx" | "page.js" | "page.mdx" => RouteKind::Page,
                    "layout.tsx" | "layout.ts" | "layout.jsx" | "layout.js" => RouteKind::Layout,
                    "loading.tsx" | "loading.ts" | "loading.jsx" | "loading.js" => {
                        RouteKind::Loading
                    }
                    "error.tsx" | "error.ts" | "error.jsx" | "error.js" => RouteKind::Error,
                    "not-found.tsx" | "not-found.ts" | "not-found.jsx" | "not-found.js" => {
                        RouteKind::NotFound
                    }
                    "template.tsx" | "template.ts" | "template.jsx" | "template.js" => {
                        RouteKind::Template
                    }
                    "default.tsx" | "default.ts" | "default.jsx" | "default.js" => {
                        RouteKind::Default
                    }
                    "global-error.tsx" | "global-error.ts" | "global-error.jsx"
                    | "global-error.js" => RouteKind::GlobalError,
                    "route.ts" | "route.js" | "route.tsx" | "route.jsx" => RouteKind::RouteHandler,
                    _ => continue,
                };

                let route_path = if prefix.is_empty() {
                    "/".to_string()
                } else {
                    prefix.to_string()
                };

                self.routes.push(Route {
                    path: route_path,
                    file: rel_path(&self.root, &path),
                    kind,
                    params: accumulated_params.to_vec(),
                    catch_all: false,
                    catch_all_optional: false,
                    slot: current_slot.clone(),
                    intercept: current_intercept,
                });
            }
        }

        Ok(())
    }

    /// Discover routes from pages/ directory (Pages Router)
    /// `current_slot` is the parallel route slot name inherited from a parent `@slot` directory
    /// `current_intercept` is the intercepting route kind inherited from a parent intercept directory
    fn discover_pages_routes(
        &mut self,
        dir: &Path,
        prefix: &str,
        current_slot: Option<String>,
        current_intercept: Option<InterceptKind>,
    ) -> Result<()> {
        let entries = std::fs::read_dir(dir)?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                let dir_info = parse_dir_name(&name);

                // Compute slot/intercept for this directory level (same logic as App Router)
                let new_slot = if name.starts_with('@') {
                    Some(name.trim_start_matches('@').to_string())
                } else {
                    current_slot.clone()
                };
                let new_intercept = parse_intercept(&name).or(current_intercept);

                let new_prefix = match &dir_info.segment {
                    DirSegment::None => prefix.to_string(),
                    DirSegment::Static(seg) => join_path(prefix, seg),
                    DirSegment::Dynamic(param) => join_path(prefix, &format!(":{}", param)),
                    DirSegment::CatchAll(param) => join_path(prefix, &format!("*{}", param)),
                    DirSegment::CatchAllOptional(param) => {
                        join_path(prefix, &format!("*?{}", param))
                    }
                };

                let mut child_params = Vec::new();
                if let Some(p) = &dir_info.param {
                    child_params.push(p.clone());
                }

                // Check for index file in this directory
                let index_exts = [
                    "index.tsx",
                    "index.ts",
                    "index.jsx",
                    "index.js",
                    "index.mdx",
                ];
                let index_file = find_file(&path, &index_exts);
                if index_file.exists() {
                    let is_api = name == "api" || prefix.ends_with("/api");
                    self.routes.push(Route {
                        path: new_prefix.clone(),
                        file: rel_path(&self.root, &index_file),
                        kind: if is_api {
                            RouteKind::Api
                        } else {
                            RouteKind::Page
                        },
                        params: child_params.clone(),
                        catch_all: dir_info.is_catch_all,
                        catch_all_optional: dir_info.is_catch_all_optional,
                        slot: new_slot.clone(),
                        intercept: new_intercept,
                    });
                }

                self.discover_pages_routes(&path, &new_prefix, new_slot, new_intercept)?;
            } else {
                // File-level routes
                let (route_path, kind, file_params) = if name == "index.tsx"
                    || name == "index.ts"
                    || name == "index.jsx"
                    || name == "index.js"
                    || name == "index.mdx"
                {
                    (
                        if prefix.is_empty() {
                            "/".to_string()
                        } else {
                            prefix.to_string()
                        },
                        if prefix.ends_with("/api") || prefix == "/api" {
                            RouteKind::Api
                        } else {
                            RouteKind::Page
                        },
                        Vec::new(),
                    )
                } else if name.ends_with(".tsx")
                    || name.ends_with(".ts")
                    || name.ends_with(".jsx")
                    || name.ends_with(".js")
                    || name.ends_with(".mdx")
                {
                    let stem = name.split('.').next().unwrap_or(&name);
                    let (seg, param) = if stem.starts_with('[') && stem.ends_with(']') {
                        let inner = &stem[1..stem.len() - 1];
                        if let Some(p) = inner.strip_prefix("...") {
                            // Catch-all in Pages Router: [...slug].tsx
                            let p = p.to_string();
                            (format!("*{}", p), Some(p))
                        } else {
                            // Regular dynamic: [param] in Pages Router file name
                            (format!(":{}", inner), Some(inner.to_string()))
                        }
                    } else {
                        (stem.to_string(), None)
                    };
                    let rp = if prefix.is_empty() {
                        format!("/{}", seg)
                    } else {
                        format!("{}/{}", prefix, seg)
                    };
                    let mut params = Vec::new();
                    if let Some(p) = param {
                        params.push(p);
                    }
                    let is_api = prefix.ends_with("/api") || prefix == "/api";
                    (
                        rp,
                        if is_api {
                            RouteKind::Api
                        } else {
                            RouteKind::Page
                        },
                        params,
                    )
                } else {
                    continue;
                };

                self.routes.push(Route {
                    path: route_path,
                    file: rel_path(&self.root, &path),
                    kind,
                    params: file_params,
                    catch_all: false,
                    catch_all_optional: false,
                    slot: current_slot.clone(),
                    intercept: current_intercept,
                });
            }
        }

        Ok(())
    }

    /// Generate a client-side router from discovered routes
    pub fn generate_router_code(&self) -> String {
        let mut code = String::new();
        code.push_str("// Auto-generated by Pledge Next.js adapter\n");
        code.push_str("// File-based routing\n\n");

        // Generate route map (pages and route handlers)
        code.push_str("const routes = {\n");
        for route in &self.routes {
            if route.kind == RouteKind::Page {
                code.push_str(&format!(
                    "  '{}': () => import('/{}'),\n",
                    route.path,
                    pledgepack_core::normalize_path_str(&route.file)
                ));
            }
        }
        code.push_str("};\n\n");

        // Generate API route map (route handlers)
        code.push_str("const apiRoutes = {\n");
        for route in &self.routes {
            if route.kind == RouteKind::RouteHandler || route.kind == RouteKind::Api {
                code.push_str(&format!(
                    "  '{}': () => import('/{}'),\n",
                    route.path,
                    pledgepack_core::normalize_path_str(&route.file)
                ));
            }
        }
        code.push_str("};\n\n");

        // Generate layout map
        code.push_str("const layouts = {\n");
        for route in &self.routes {
            if route.kind == RouteKind::Layout || route.kind == RouteKind::Template {
                code.push_str(&format!(
                    "  '{}': () => import('/{}'),\n",
                    route.path,
                    pledgepack_core::normalize_path_str(&route.file)
                ));
            }
        }
        code.push_str("};\n\n");

        // Generate router
        code.push_str(
            r#"export function navigate(path) {
  const route = routes[path];
  if (route) {
    history.pushState({}, '', path);
    window.dispatchEvent(new PopStateEvent('popstate', { state: {} }));
  }
}

window.addEventListener('popstate', () => {
  const path = window.location.pathname;
  const route = routes[path];
  if (route) {
    route().then(mod => {
      const app = document.getElementById('__next');
      if (app && mod.default) {
        app.innerHTML = '';
        if (typeof mod.default === 'function') {
          mod.default(app);
        }
      }
    });
  }
});

export function getRoutes() {
  return Object.keys(routes);
}

export function getApiRoutes() {
  return Object.keys(apiRoutes);
}

export function getLayouts() {
  return Object.keys(layouts);
}
"#,
        );

        code
    }

    /// Generate SSR manifest for server-side rendering
    pub fn generate_ssr_manifest(&self) -> String {
        let manifest: Vec<serde_json::Value> = self
            .routes
            .iter()
            .map(|r| {
                serde_json::json!({
                    "path": r.path,
                    "file": r.file,
                    "kind": format!("{:?}", r.kind),
                    "params": r.params,
                    "catchAll": r.catch_all,
                    "catchAllOptional": r.catch_all_optional,
                })
            })
            .collect();

        let mut root = serde_json::Map::new();
        root.insert("routes".to_string(), serde_json::Value::Array(manifest));
        if let Some(mw) = &self.middleware {
            root.insert(
                "middleware".to_string(),
                serde_json::Value::String(mw.clone()),
            );
        }
        root.insert(
            "routerType".to_string(),
            serde_json::Value::String(format!("{:?}", self.router_type)),
        );

        serde_json::to_string_pretty(&serde_json::Value::Object(root)).unwrap_or("{}".to_string())
    }

    /// Get all page routes (for static generation)
    pub fn get_page_routes(&self) -> Vec<&Route> {
        self.routes
            .iter()
            .filter(|r| r.kind == RouteKind::Page)
            .collect()
    }

    /// Get all API/route handler routes
    pub fn get_api_routes(&self) -> Vec<&Route> {
        self.routes
            .iter()
            .filter(|r| r.kind == RouteKind::Api || r.kind == RouteKind::RouteHandler)
            .collect()
    }

    /// Get all layout routes (including templates)
    pub fn get_layouts(&self) -> Vec<&Route> {
        self.routes
            .iter()
            .filter(|r| r.kind == RouteKind::Layout || r.kind == RouteKind::Template)
            .collect()
    }

    /// Validate route+page coexistence (Next.js disallows route.ts + page.ts at same level)
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for route in &self.routes {
            if route.kind == RouteKind::Page || route.kind == RouteKind::RouteHandler {
                let count = seen.entry(&route.path).or_insert(0);
                *count += 1;
                if *count > 1 {
                    warnings.push(format!(
                        "Route '{}' has both page and route handler — Next.js disallows this",
                        route.path
                    ));
                }
            }
        }
        warnings
    }
}

/// Find the first existing file from a list of candidate names in a directory
fn find_file(dir: &Path, candidates: &[&str]) -> PathBuf {
    for name in candidates {
        let path = dir.join(name);
        if path.exists() {
            return path;
        }
    }
    dir.join(candidates[0])
}

/// Get relative path from root, with forward slashes
fn rel_path(root: &Path, abs: &Path) -> String {
    pledgepack_core::normalize_path(abs.strip_prefix(root).unwrap_or(abs))
}

/// Join a prefix with a segment, handling empty prefix
fn join_path(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() {
        format!("/{}", segment)
    } else {
        format!("{}/{}", prefix, segment)
    }
}

/// Parsed directory name for routing
struct DirInfo {
    segment: DirSegment,
    param: Option<String>,
    is_catch_all: bool,
    is_catch_all_optional: bool,
}

enum DirSegment {
    None,
    Static(String),
    Dynamic(String),
    CatchAll(String),
    CatchAllOptional(String),
}

/// Parse a directory name into routing information
fn parse_dir_name(name: &str) -> DirInfo {
    // Parallel route: @slot
    if name.starts_with('@') {
        return DirInfo {
            segment: DirSegment::None,
            param: None,
            is_catch_all: false,
            is_catch_all_optional: false,
        };
    }

    // Route group: (group) — doesn't affect URL
    if name.starts_with('(') && name.ends_with(')') && !name.starts_with("(..)") {
        // Check for intercepting route: (.), (..), (..)(..), (...)
        if parse_intercept(name).is_some() {
            // Intercepting route — extract the actual route name after the intercept prefix
            let rest = &name[intercept_prefix_len(name)..];
            if rest.starts_with('[') && rest.ends_with(']') {
                return parse_dynamic(rest);
            }
            return DirInfo {
                segment: DirSegment::Static(rest.to_string()),
                param: None,
                is_catch_all: false,
                is_catch_all_optional: false,
            };
        }
        // Regular route group
        return DirInfo {
            segment: DirSegment::None,
            param: None,
            is_catch_all: false,
            is_catch_all_optional: false,
        };
    }

    // Intercepting route: (..)segment, (...)segment
    if name.starts_with("(..)") || name.starts_with("(...)") {
        let prefix_len = intercept_prefix_len(name);
        let rest = &name[prefix_len..];
        if rest.starts_with('[') && rest.ends_with(']') {
            return parse_dynamic(rest);
        }
        return DirInfo {
            segment: DirSegment::Static(rest.to_string()),
            param: None,
            is_catch_all: false,
            is_catch_all_optional: false,
        };
    }

    // Dynamic route: [param]
    if name.starts_with('[') && name.ends_with(']') {
        return parse_dynamic(name);
    }

    // Static route
    DirInfo {
        segment: DirSegment::Static(name.to_string()),
        param: None,
        is_catch_all: false,
        is_catch_all_optional: false,
    }
}

/// Parse dynamic segment: [param], [...slug], [[...slug]]
fn parse_dynamic(name: &str) -> DirInfo {
    // Optional catch-all: [[...slug]] — double brackets
    if name.starts_with("[[") && name.ends_with("]]") {
        let inner = &name[2..name.len() - 2];
        if let Some(param) = inner.strip_prefix("...") {
            let param = param.to_string();
            return DirInfo {
                segment: DirSegment::CatchAllOptional(param.clone()),
                param: Some(param),
                is_catch_all: false,
                is_catch_all_optional: true,
            };
        }
    }

    // Single bracket: [param] or [...slug]
    let inner = &name[1..name.len() - 1];

    // Catch-all: [...slug]
    if let Some(param) = inner.strip_prefix("...") {
        let param = param.to_string();
        return DirInfo {
            segment: DirSegment::CatchAll(param.clone()),
            param: Some(param),
            is_catch_all: true,
            is_catch_all_optional: false,
        };
    }

    // Regular dynamic: [param]
    DirInfo {
        segment: DirSegment::Dynamic(inner.to_string()),
        param: Some(inner.to_string()),
        is_catch_all: false,
        is_catch_all_optional: false,
    }
}

/// Parse intercepting route prefix
fn parse_intercept(name: &str) -> Option<InterceptKind> {
    if name.starts_with("(...)") {
        Some(InterceptKind::Root)
    } else if name.starts_with("(..)(..)") {
        Some(InterceptKind::GrandParent)
    } else if name.starts_with("(..)") {
        Some(InterceptKind::Parent)
    } else if name.starts_with("(.)") {
        Some(InterceptKind::Same)
    } else {
        None
    }
}

/// Get the length of the intercept prefix in the directory name
fn intercept_prefix_len(name: &str) -> usize {
    if name.starts_with("(...)") {
        5
    } else if name.starts_with("(..)(..)") {
        8
    } else if name.starts_with("(..)") {
        4
    } else if name.starts_with("(.)") {
        3
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_temp_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // App Router structure
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("app/posts/[id]")).unwrap();
        fs::create_dir_all(root.join("app/posts")).unwrap();
        fs::create_dir_all(root.join("app/docs/[...slug]")).unwrap();
        fs::create_dir_all(root.join("app/blog/[[...slug]]")).unwrap();
        fs::create_dir_all(root.join("app/(marketing)")).unwrap();
        fs::create_dir_all(root.join("app/(marketing)/about")).unwrap();
        fs::create_dir_all(root.join("app/dashboard/@analytics")).unwrap();
        fs::create_dir_all(root.join("app/api/users")).unwrap();

        // Root page and layout
        fs::write(
            root.join("app/page.tsx"),
            "export default function Page() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/layout.tsx"),
            "export default function Layout() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/loading.tsx"),
            "export default function Loading() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/error.tsx"),
            "export default function Error() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/not-found.tsx"),
            "export default function NotFound() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/global-error.tsx"),
            "export default function GlobalError() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/template.tsx"),
            "export default function Template() {}",
        )
        .unwrap();

        // Posts page
        fs::write(
            root.join("app/posts/page.tsx"),
            "export default function Posts() {}",
        )
        .unwrap();

        // Dynamic route [id]
        fs::write(
            root.join("app/posts/[id]/page.tsx"),
            "export default function Post() {}",
        )
        .unwrap();

        // Catch-all [...slug]
        fs::write(
            root.join("app/docs/[...slug]/page.tsx"),
            "export default function Doc() {}",
        )
        .unwrap();

        // Optional catch-all [[...slug]]
        fs::write(
            root.join("app/blog/[[...slug]]/page.tsx"),
            "export default function Blog() {}",
        )
        .unwrap();

        // Route group (marketing) — doesn't affect URL
        fs::write(
            root.join("app/(marketing)/about/page.tsx"),
            "export default function About() {}",
        )
        .unwrap();

        // Parallel route @analytics
        fs::write(
            root.join("app/dashboard/@analytics/default.tsx"),
            "export default function Default() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/dashboard/page.tsx"),
            "export default function Dashboard() {}",
        )
        .unwrap();

        // Route handler (API)
        fs::write(
            root.join("app/api/users/route.ts"),
            "export async function GET() {}",
        )
        .unwrap();

        // Middleware
        fs::write(
            root.join("middleware.ts"),
            "export function middleware() {}",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_dynamic_route_parsing() {
        let info = parse_dir_name("[id]");
        assert!(matches!(info.segment, DirSegment::Dynamic(ref s) if s == "id"));
        assert_eq!(info.param, Some("id".to_string()));
        assert!(!info.is_catch_all);
        assert!(!info.is_catch_all_optional);
    }

    #[test]
    fn test_catch_all_parsing() {
        let info = parse_dir_name("[...slug]");
        assert!(matches!(info.segment, DirSegment::CatchAll(ref s) if s == "slug"));
        assert_eq!(info.param, Some("slug".to_string()));
        assert!(info.is_catch_all);
        assert!(!info.is_catch_all_optional);
    }

    #[test]
    fn test_optional_catch_all_parsing() {
        let info = parse_dir_name("[[...slug]]");
        assert!(matches!(info.segment, DirSegment::CatchAllOptional(ref s) if s == "slug"));
        assert_eq!(info.param, Some("slug".to_string()));
        assert!(!info.is_catch_all);
        assert!(info.is_catch_all_optional);
    }

    #[test]
    fn test_route_group_parsing() {
        let info = parse_dir_name("(marketing)");
        assert!(matches!(info.segment, DirSegment::None));
        assert_eq!(info.param, None);
    }

    #[test]
    fn test_parallel_route_parsing() {
        let info = parse_dir_name("@analytics");
        assert!(matches!(info.segment, DirSegment::None));
        assert_eq!(info.param, None);
    }

    #[test]
    fn test_intercept_parsing() {
        assert_eq!(parse_intercept("(.)photos"), Some(InterceptKind::Same));
        assert_eq!(parse_intercept("(..)photos"), Some(InterceptKind::Parent));
        assert_eq!(
            parse_intercept("(..)(..)photos"),
            Some(InterceptKind::GrandParent)
        );
        assert_eq!(parse_intercept("(...)photos"), Some(InterceptKind::Root));
        assert_eq!(parse_intercept("photos"), None);
    }

    #[test]
    fn test_intercept_prefix_len() {
        assert_eq!(intercept_prefix_len("(.)photos"), 3);
        assert_eq!(intercept_prefix_len("(..)photos"), 4);
        assert_eq!(intercept_prefix_len("(..)(..)photos"), 8);
        assert_eq!(intercept_prefix_len("(...)photos"), 5);
        assert_eq!(intercept_prefix_len("photos"), 0);
    }

    #[test]
    fn test_static_dir_parsing() {
        let info = parse_dir_name("about");
        assert!(matches!(info.segment, DirSegment::Static(ref s) if s == "about"));
        assert_eq!(info.param, None);
    }

    #[test]
    fn test_full_app_router_discovery() {
        let dir = create_temp_project();
        let mut adapter = NextAdapter::new(dir.path());
        adapter.discover_routes().unwrap();

        assert_eq!(adapter.router_type, RouterType::AppRouter);

        // Should find middleware
        assert!(adapter.middleware.is_some());
        assert_eq!(adapter.middleware.as_ref().unwrap(), "middleware.ts");

        // Should find root page
        let pages = adapter.get_page_routes();
        let paths: Vec<&str> = pages.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/"), "root page not found");
        assert!(paths.contains(&"/posts"), "posts page not found");
        assert!(paths.contains(&"/posts/:id"), "dynamic [id] page not found");
        assert!(
            paths.contains(&"/docs/*slug"),
            "catch-all [...slug] page not found"
        );
        assert!(
            paths.contains(&"/blog/*?slug"),
            "optional catch-all [[...slug]] page not found"
        );
        assert!(
            paths.contains(&"/about"),
            "route group (marketing)/about page not found"
        );
        assert!(paths.contains(&"/dashboard"), "dashboard page not found");
    }

    #[test]
    fn test_route_handler_discovery() {
        let dir = create_temp_project();
        let mut adapter = NextAdapter::new(dir.path());
        adapter.discover_routes().unwrap();

        let api_routes = adapter.get_api_routes();
        let paths: Vec<&str> = api_routes.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"/api/users"),
            "route handler /api/users not found, got: {:?}",
            paths
        );
    }

    #[test]
    fn test_layout_and_template_discovery() {
        let dir = create_temp_project();
        let mut adapter = NextAdapter::new(dir.path());
        adapter.discover_routes().unwrap();

        let layouts = adapter.get_layouts();
        let paths: Vec<&str> = layouts.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/"), "root layout not found");
    }

    #[test]
    fn test_file_conventions() {
        let dir = create_temp_project();
        let mut adapter = NextAdapter::new(dir.path());
        adapter.discover_routes().unwrap();

        // Check all file convention kinds are discovered
        let kinds: Vec<RouteKind> = adapter.routes.iter().map(|r| r.kind).collect();
        assert!(kinds.contains(&RouteKind::Page), "page convention missing");
        assert!(
            kinds.contains(&RouteKind::Layout),
            "layout convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::Loading),
            "loading convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::Error),
            "error convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::NotFound),
            "not-found convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::GlobalError),
            "global-error convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::Template),
            "template convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::Default),
            "default convention missing"
        );
        assert!(
            kinds.contains(&RouteKind::RouteHandler),
            "route handler convention missing"
        );
    }

    #[test]
    fn test_nested_params_accumulation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create nested dynamic routes: app/shop/[category]/[item]/page.tsx
        fs::create_dir_all(root.join("app/shop/[category]/[item]")).unwrap();
        fs::write(
            root.join("app/shop/[category]/[item]/page.tsx"),
            "export default function Page() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/layout.tsx"),
            "export default function Layout() {}",
        )
        .unwrap();

        let mut adapter = NextAdapter::new(root);
        adapter.discover_routes().unwrap();

        let page = adapter
            .routes
            .iter()
            .find(|r| r.path == "/shop/:category/:item")
            .expect("nested dynamic route not found");
        assert_eq!(
            page.params,
            vec!["category", "item"],
            "params should accumulate from parent directories"
        );
    }

    #[test]
    fn test_pages_router_api_routes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Pages Router structure with API routes
        fs::create_dir_all(root.join("pages/api/users")).unwrap();
        fs::write(
            root.join("pages/api/users/index.ts"),
            "export default function handler() {}",
        )
        .unwrap();
        fs::write(
            root.join("pages/index.tsx"),
            "export default function Page() {}",
        )
        .unwrap();

        let mut adapter = NextAdapter::new(root);
        adapter.discover_routes().unwrap();

        let api_routes = adapter.get_api_routes();
        let paths: Vec<&str> = api_routes.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"/api/users"),
            "pages/api/users should be an API route, got: {:?}",
            paths
        );
    }

    #[test]
    fn test_src_directory_support() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // src/app structure
        fs::create_dir_all(root.join("src/app/about")).unwrap();
        fs::write(
            root.join("src/app/page.tsx"),
            "export default function Page() {}",
        )
        .unwrap();
        fs::write(
            root.join("src/app/layout.tsx"),
            "export default function Layout() {}",
        )
        .unwrap();
        fs::write(
            root.join("src/app/about/page.tsx"),
            "export default function About() {}",
        )
        .unwrap();
        fs::write(
            root.join("src/middleware.ts"),
            "export function middleware() {}",
        )
        .unwrap();

        let mut adapter = NextAdapter::new(root);
        adapter.discover_routes().unwrap();

        assert_eq!(adapter.router_type, RouterType::AppRouter);
        assert!(adapter.middleware.is_some());

        let pages = adapter.get_page_routes();
        let paths: Vec<&str> = pages.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/"), "root page from src/app not found");
        assert!(
            paths.contains(&"/about"),
            "about page from src/app not found"
        );
    }

    #[test]
    fn test_mdx_support() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(root.join("app/page.mdx"), "# Hello").unwrap();
        fs::write(
            root.join("app/layout.tsx"),
            "export default function Layout() {}",
        )
        .unwrap();

        let mut adapter = NextAdapter::new(root);
        adapter.discover_routes().unwrap();

        let pages = adapter.get_page_routes();
        assert!(
            pages
                .iter()
                .any(|r| r.path == "/" && r.file.ends_with(".mdx")),
            "MDX page not found"
        );
    }

    #[test]
    fn test_ssr_manifest_includes_middleware() {
        let dir = create_temp_project();
        let mut adapter = NextAdapter::new(dir.path());
        adapter.discover_routes().unwrap();

        let manifest = adapter.generate_ssr_manifest();
        assert!(
            manifest.contains("middleware"),
            "manifest should include middleware"
        );
        assert!(
            manifest.contains("AppRouter"),
            "manifest should include router type"
        );
        assert!(
            manifest.contains("catchAll"),
            "manifest should include catchAll field"
        );
    }

    #[test]
    fn test_validate_route_page_coexistence() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("app/test")).unwrap();
        fs::write(
            root.join("app/test/page.tsx"),
            "export default function Page() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/test/route.ts"),
            "export async function GET() {}",
        )
        .unwrap();
        fs::write(
            root.join("app/layout.tsx"),
            "export default function Layout() {}",
        )
        .unwrap();

        let mut adapter = NextAdapter::new(root);
        adapter.discover_routes().unwrap();

        let warnings = adapter.validate();
        assert!(
            !warnings.is_empty(),
            "should warn about page+route coexistence"
        );
    }

    #[test]
    fn test_router_code_generation() {
        let dir = create_temp_project();
        let mut adapter = NextAdapter::new(dir.path());
        adapter.discover_routes().unwrap();

        let code = adapter.generate_router_code();
        assert!(
            code.contains("const routes = {"),
            "should generate routes map"
        );
        assert!(
            code.contains("const apiRoutes = {"),
            "should generate apiRoutes map"
        );
        assert!(
            code.contains("const layouts = {"),
            "should generate layouts map"
        );
        assert!(code.contains("getApiRoutes"), "should export getApiRoutes");
        assert!(code.contains("getLayouts"), "should export getLayouts");
    }
}
