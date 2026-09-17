// Route tracker — route-level lazy bundling and eviction.
//
// Turbopack only compiles routes you visit. PledgePack's dev server currently
// transforms modules on-demand (module-level lazy), but doesn't track which
// modules belong to which route or evict routes you've navigated away from.
//
// The `RouteTracker` provides:
//   1. Route → module set mapping: which modules were compiled for each route.
//   2. Active route tracking: which routes are currently being visited.
//   3. LRU eviction: when memory pressure exceeds a threshold, evict the
//      least-recently-used route's module caches.
//   4. Route prediction: based on navigation history, pre-compute likely
//      next routes.
//   5. Prefetch-on-hover: the browser sends a "prefetch" message for a route
//      when the user hovers over a `<Link>`, and the server starts computing
//      that route's tasks.
//
// # Integration
//
// The `RouteTracker` is used by the dev server:
//   - When a module request comes in, the route is inferred from the
//     `Referer` header or the URL path.
//   - The module is added to the route's module set.
//   - When a WebSocket "prefetch" message is received, the route is marked
//     as prefetched and its tasks are scheduled.
//   - Periodically (or on memory pressure), LRU eviction runs.

use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::RwLock;
use tracing::{debug, info};

/// A route entry in the tracker.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// The route path (e.g., "/", "/about", "/dashboard/settings").
    pub path: String,
    /// Module paths that were compiled for this route.
    pub modules: HashSet<String>,
    /// Last-accessed timestamp (Unix millis).
    pub last_accessed: u64,
    /// Whether this route is currently active (being visited).
    pub is_active: bool,
    /// Whether this route was prefetched (hover-prefetch).
    pub is_prefetched: bool,
    /// Number of times this route has been visited.
    pub visit_count: u32,
}

/// Configuration for the route tracker.
#[derive(Debug, Clone)]
pub struct RouteTrackerConfig {
    /// Maximum number of routes to keep in memory before LRU eviction.
    pub max_routes: usize,
    /// Maximum total modules across all routes before eviction.
    pub max_total_modules: usize,
    /// Whether route prediction is enabled.
    pub enable_prediction: bool,
    /// Number of navigation history entries to keep for prediction.
    pub prediction_history_size: usize,
}

impl Default for RouteTrackerConfig {
    fn default() -> Self {
        RouteTrackerConfig {
            max_routes: 20,
            max_total_modules: 500,
            enable_prediction: true,
            prediction_history_size: 50,
        }
    }
}

/// Route-level lazy bundling tracker.
///
/// Tracks which modules belong to which route, which routes are active,
/// and supports LRU eviction of route module caches.
pub struct RouteTracker {
    /// Route path → route entry.
    routes: RwLock<HashMap<String, RouteEntry>>,
    /// G6.10: Module path → set of route paths that use this module (shared module tracking).
    module_to_routes: RwLock<HashMap<String, HashSet<String>>>,
    /// Navigation history for route prediction.
    nav_history: RwLock<VecDeque<String>>,
    /// Configuration.
    config: RouteTrackerConfig,
}

impl RouteTracker {
    /// Create a new route tracker with the given configuration.
    pub fn new(config: RouteTrackerConfig) -> Self {
        RouteTracker {
            routes: RwLock::new(HashMap::new()),
            module_to_routes: RwLock::new(HashMap::new()),
            nav_history: RwLock::new(VecDeque::new()),
            config,
        }
    }

    /// Create a new route tracker with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(RouteTrackerConfig::default())
    }

    /// Record a module request for a route.
    ///
    /// Called when the dev server serves a module. The route is inferred
    /// from the request URL or Referer header.
    pub async fn record_module(&self, route_path: &str, module_path: &str) {
        let now = current_millis();
        let mut routes = self.routes.write().await;
        let mut module_map = self.module_to_routes.write().await;

        let entry = routes
            .entry(route_path.to_string())
            .or_insert_with(|| RouteEntry {
                path: route_path.to_string(),
                modules: HashSet::new(),
                last_accessed: now,
                is_active: false,
                is_prefetched: false,
                visit_count: 0,
            });

        entry.last_accessed = now;
        entry.modules.insert(module_path.to_string());

        // G6.10: Track which routes use this module (shared module caching).
        // A module can be shared across multiple routes — only evicted when
        // no route uses it.
        module_map
            .entry(module_path.to_string())
            .or_default()
            .insert(route_path.to_string());

        debug!(
            "Route '{}' now has {} modules (added: {})",
            route_path,
            entry.modules.len(),
            module_path
        );
    }

    /// Mark a route as active (being visited by the browser).
    pub async fn mark_active(&self, route_path: &str) {
        let now = current_millis();
        let mut routes = self.routes.write().await;

        let entry = routes
            .entry(route_path.to_string())
            .or_insert_with(|| RouteEntry {
                path: route_path.to_string(),
                modules: HashSet::new(),
                last_accessed: now,
                is_active: true,
                is_prefetched: false,
                visit_count: 0,
            });

        entry.is_active = true;
        entry.last_accessed = now;
        entry.visit_count += 1;

        // Add to navigation history
        if self.config.enable_prediction {
            let mut history = self.nav_history.write().await;
            history.push_back(route_path.to_string());
            while history.len() > self.config.prediction_history_size {
                history.pop_front();
            }
        }

        info!(
            "Route '{}' marked active (visits: {})",
            route_path, entry.visit_count
        );
    }

    /// Mark a route as inactive (user navigated away).
    pub async fn mark_inactive(&self, route_path: &str) {
        let mut routes = self.routes.write().await;
        if let Some(entry) = routes.get_mut(route_path) {
            entry.is_active = false;
            debug!("Route '{}' marked inactive", route_path);
        }
    }

    /// Mark a route as prefetched (hover-prefetch).
    ///
    /// The browser sends a prefetch message when the user hovers over a link.
    /// The server can start computing the route's tasks speculatively.
    pub async fn mark_prefetched(&self, route_path: &str) {
        let now = current_millis();
        let mut routes = self.routes.write().await;

        let entry = routes
            .entry(route_path.to_string())
            .or_insert_with(|| RouteEntry {
                path: route_path.to_string(),
                modules: HashSet::new(),
                last_accessed: now,
                is_active: false,
                is_prefetched: true,
                visit_count: 0,
            });

        entry.is_prefetched = true;
        entry.last_accessed = now;

        info!("Route '{}' prefetched (hover)", route_path);
    }

    /// Get the routes that use a given module path.
    ///
    /// G6.10: A module can be shared across multiple routes.
    /// Returns all routes that reference this module.
    pub async fn routes_for_module(&self, module_path: &str) -> HashSet<String> {
        self.module_to_routes
            .read()
            .await
            .get(module_path)
            .cloned()
            .unwrap_or_default()
    }

    /// Get the primary route for a given module path (first route that uses it).
    ///
    /// Kept for backwards compatibility — prefer `routes_for_module` for G6.10.
    pub async fn route_for_module(&self, module_path: &str) -> Option<String> {
        self.module_to_routes
            .read()
            .await
            .get(module_path)
            .and_then(|routes| routes.iter().next().cloned())
    }

    /// Get all modules for a route.
    pub async fn modules_for_route(&self, route_path: &str) -> Option<HashSet<String>> {
        self.routes
            .read()
            .await
            .get(route_path)
            .map(|e| e.modules.clone())
    }

    /// Get all active routes.
    pub async fn active_routes(&self) -> Vec<String> {
        self.routes
            .read()
            .await
            .iter()
            .filter(|(_, e)| e.is_active)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Get all prefetched routes.
    pub async fn prefetched_routes(&self) -> Vec<String> {
        self.routes
            .read()
            .await
            .iter()
            .filter(|(_, e)| e.is_prefetched && !e.is_active)
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Predict the next likely routes based on navigation history.
    ///
    /// Uses simple frequency analysis: which routes commonly follow the
    /// current route in the navigation history?
    pub async fn predict_next_routes(&self, current_route: &str) -> Vec<String> {
        if !self.config.enable_prediction {
            return Vec::new();
        }

        let history = self.nav_history.read().await;
        let mut transitions: HashMap<String, u32> = HashMap::new();

        // Find all transitions from current_route to other routes
        let mut found_current = false;
        for route in history.iter() {
            if found_current {
                *transitions.entry(route.clone()).or_default() += 1;
                found_current = false;
            }
            if route == current_route {
                found_current = true;
            }
        }

        // Sort by frequency
        let mut predictions: Vec<(String, u32)> = transitions.into_iter().collect();
        predictions.sort_by(|a, b| b.1.cmp(&a.1));

        predictions.into_iter().map(|(r, _)| r).collect()
    }

    /// Evict least-recently-used routes to stay within memory limits.
    ///
    /// G6.10: When evicting a route, shared modules (used by other routes)
    /// are NOT removed from the module cache — only the route's reference is
    /// removed. A module is fully evicted only when no route uses it.
    ///
    /// Returns the list of evicted route paths and their exclusive modules
    /// (modules that were only used by this route and are now fully evicted).
    /// Active routes are never evicted.
    pub async fn evict_lru(&self) -> Vec<(String, HashSet<String>)> {
        let mut routes = self.routes.write().await;
        let mut module_map = self.module_to_routes.write().await;

        let total_modules: usize = routes.values().map(|e| e.modules.len()).sum();

        let mut evicted = Vec::new();

        // Check if we need to evict
        if routes.len() <= self.config.max_routes && total_modules <= self.config.max_total_modules
        {
            return evicted;
        }

        // Sort routes by last_accessed (ascending = oldest first)
        // Exclude active routes from eviction
        let mut candidates: Vec<(String, u64)> = routes
            .iter()
            .filter(|(_, e)| !e.is_active)
            .map(|(k, e)| (k.clone(), e.last_accessed))
            .collect();
        candidates.sort_by_key(|(_, t)| *t);

        // Evict until we're within limits
        for (route_path, _) in candidates {
            if routes.len() <= self.config.max_routes
                && routes.values().map(|e| e.modules.len()).sum::<usize>()
                    <= self.config.max_total_modules
            {
                break;
            }

            if let Some(entry) = routes.remove(&route_path) {
                // G6.10: Only fully evict modules that are NOT shared with other routes.
                // Remove this route's reference from each module's route set.
                // If the module has no remaining routes, it's fully evicted.
                let mut exclusive_modules = HashSet::new();

                for module in &entry.modules {
                    if let Some(route_set) = module_map.get_mut(module) {
                        route_set.remove(&route_path);
                        if route_set.is_empty() {
                            module_map.remove(module);
                            exclusive_modules.insert(module.clone());
                        }
                    }
                }

                evicted.push((route_path.clone(), exclusive_modules));
                info!(
                    "Route '{}' evicted (LRU) — {} exclusive modules removed, {} shared modules retained",
                    route_path,
                    evicted.last().map(|(_, m)| m.len()).unwrap_or(0),
                    entry.modules.len() - evicted.last().map(|(_, m)| m.len()).unwrap_or(0),
                );
            }
        }

        evicted
    }

    /// Get the number of tracked routes.
    pub async fn route_count(&self) -> usize {
        self.routes.read().await.len()
    }

    /// Get the total number of tracked modules across all routes.
    pub async fn total_modules(&self) -> usize {
        self.routes
            .read()
            .await
            .values()
            .map(|e| e.modules.len())
            .sum()
    }

    /// G6.10: Get the number of modules shared across multiple routes.
    ///
    /// A module is "shared" if it appears in 2 or more routes.
    /// These modules are computed once and reused, saving memory and compute.
    pub async fn shared_modules_count(&self) -> usize {
        self.module_to_routes
            .read()
            .await
            .values()
            .filter(|routes| routes.len() > 1)
            .count()
    }

    /// G6.10: Get the number of unique modules tracked across all routes.
    pub async fn unique_modules_count(&self) -> usize {
        self.module_to_routes.read().await.len()
    }

    /// Get a snapshot of all routes (for debugging/inspection).
    pub async fn all_routes(&self) -> Vec<RouteEntry> {
        self.routes.read().await.values().cloned().collect()
    }

    /// Clear all routes (for full rebuilds).
    pub async fn clear(&self) {
        self.routes.write().await.clear();
        self.module_to_routes.write().await.clear();
        self.nav_history.write().await.clear();
    }
}

/// Get the current time in milliseconds since Unix epoch.
fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_module_creates_route() {
        let tracker = RouteTracker::with_defaults();
        tracker.record_module("/", "/src/index.tsx").await;
        tracker.record_module("/", "/src/App.tsx").await;

        let modules = tracker.modules_for_route("/").await.unwrap();
        assert_eq!(modules.len(), 2);
        assert!(modules.contains("/src/index.tsx"));
        assert!(modules.contains("/src/App.tsx"));
    }

    #[tokio::test]
    async fn mark_active_increments_visit_count() {
        let tracker = RouteTracker::with_defaults();

        tracker.mark_active("/about").await;
        tracker.mark_active("/about").await;
        tracker.mark_active("/about").await;

        let routes = tracker.all_routes().await;
        let about = routes.iter().find(|r| r.path == "/about").unwrap();
        assert_eq!(about.visit_count, 3);
        assert!(about.is_active);
    }

    #[tokio::test]
    async fn mark_inactive_clears_active_flag() {
        let tracker = RouteTracker::with_defaults();

        tracker.mark_active("/dashboard").await;
        tracker.mark_inactive("/dashboard").await;

        let routes = tracker.all_routes().await;
        let dashboard = routes.iter().find(|r| r.path == "/dashboard").unwrap();
        assert!(!dashboard.is_active);
    }

    #[tokio::test]
    async fn mark_prefetched_sets_flag() {
        let tracker = RouteTracker::with_defaults();

        tracker.mark_prefetched("/settings").await;

        let prefetched = tracker.prefetched_routes().await;
        assert_eq!(prefetched, vec!["/settings".to_string()]);
    }

    #[tokio::test]
    async fn route_for_module_lookup() {
        let tracker = RouteTracker::with_defaults();

        tracker.record_module("/blog", "/src/pages/blog.tsx").await;
        tracker
            .record_module("/blog", "/src/components/Post.tsx")
            .await;

        assert_eq!(
            tracker.route_for_module("/src/pages/blog.tsx").await,
            Some("/blog".to_string())
        );
        assert_eq!(
            tracker.route_for_module("/src/components/Post.tsx").await,
            Some("/blog".to_string())
        );
        assert_eq!(tracker.route_for_module("/nonexistent").await, None);
    }

    #[tokio::test]
    async fn active_routes_filter() {
        let tracker = RouteTracker::with_defaults();

        tracker.mark_active("/").await;
        tracker.mark_active("/about").await;
        tracker.mark_active("/contact").await;
        tracker.mark_inactive("/about").await;

        let active = tracker.active_routes().await;
        assert_eq!(active.len(), 2);
        assert!(active.contains(&"/".to_string()));
        assert!(active.contains(&"/contact".to_string()));
        assert!(!active.contains(&"/about".to_string()));
    }

    #[tokio::test]
    async fn predict_next_routes_from_history() {
        let tracker = RouteTracker::with_defaults();

        // Simulate navigation: / → /about → /contact → / → /about → /blog
        tracker.mark_active("/").await;
        tracker.mark_active("/about").await;
        tracker.mark_active("/contact").await;
        tracker.mark_active("/").await;
        tracker.mark_active("/about").await;
        tracker.mark_active("/blog").await;

        // After "/", the most common next route is "/about"
        let predictions = tracker.predict_next_routes("/").await;
        assert!(!predictions.is_empty());
        assert_eq!(predictions[0], "/about");
    }

    #[tokio::test]
    async fn evict_lru_removes_oldest_inactive() {
        let config = RouteTrackerConfig {
            max_routes: 2,
            max_total_modules: 100,
            enable_prediction: false,
            prediction_history_size: 10,
        };
        let tracker = RouteTracker::new(config);

        // Add 3 routes, mark first as active (won't be evicted)
        tracker.record_module("/old", "/src/old.tsx").await;
        tracker.mark_active("/old").await;
        tracker.mark_inactive("/old").await;

        // Small delay to ensure different timestamps
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        tracker.record_module("/mid", "/src/mid.tsx").await;
        tracker.mark_active("/mid").await;
        tracker.mark_inactive("/mid").await;

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        tracker.record_module("/new", "/src/new.tsx").await;
        tracker.mark_active("/new").await; // active — won't be evicted

        // Evict — should remove /old (oldest inactive)
        let evicted = tracker.evict_lru().await;
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0, "/old");
        assert_eq!(tracker.route_count().await, 2);
    }

    #[tokio::test]
    async fn evict_lru_never_evicts_active() {
        let config = RouteTrackerConfig {
            max_routes: 1,
            max_total_modules: 100,
            enable_prediction: false,
            prediction_history_size: 10,
        };
        let tracker = RouteTracker::new(config);

        tracker.record_module("/active", "/src/active.tsx").await;
        tracker.mark_active("/active").await;

        // Try to evict — should not evict the active route
        let evicted = tracker.evict_lru().await;
        assert!(evicted.is_empty());
        assert_eq!(tracker.route_count().await, 1);
    }

    #[tokio::test]
    async fn clear_resets_all_state() {
        let tracker = RouteTracker::with_defaults();

        tracker.record_module("/", "/src/index.tsx").await;
        tracker.mark_active("/").await;

        tracker.clear().await;

        assert_eq!(tracker.route_count().await, 0);
        assert_eq!(tracker.total_modules().await, 0);
    }

    #[tokio::test]
    async fn total_modules_across_routes() {
        let tracker = RouteTracker::with_defaults();

        tracker.record_module("/a", "/src/a1.tsx").await;
        tracker.record_module("/a", "/src/a2.tsx").await;
        tracker.record_module("/b", "/src/b1.tsx").await;

        assert_eq!(tracker.total_modules().await, 3);
    }

    #[tokio::test]
    async fn shared_modules_are_tracked_across_routes() {
        let tracker = RouteTracker::with_defaults();

        // Two routes share a common module
        tracker.record_module("/home", "/src/home.tsx").await;
        tracker.record_module("/home", "/src/shared.tsx").await;
        tracker.record_module("/about", "/src/about.tsx").await;
        tracker.record_module("/about", "/src/shared.tsx").await;

        // shared.tsx is used by both routes
        assert_eq!(
            tracker.shared_modules_count().await,
            1,
            "Should have 1 shared module"
        );
        assert_eq!(
            tracker.unique_modules_count().await,
            3,
            "Should have 3 unique modules"
        );
        assert_eq!(
            tracker.total_modules().await,
            4,
            "Should have 4 total module references"
        );

        let routes = tracker.routes_for_module("/src/shared.tsx").await;
        assert_eq!(routes.len(), 2, "shared.tsx should be in 2 routes");
        assert!(routes.contains("/home"));
        assert!(routes.contains("/about"));
    }

    #[tokio::test]
    async fn eviction_retains_shared_modules() {
        let config = RouteTrackerConfig {
            max_routes: 2,
            max_total_modules: 100,
            enable_prediction: false,
            prediction_history_size: 10,
        };
        let tracker = RouteTracker::new(config);

        // /old shares a module with /keep
        tracker.record_module("/old", "/src/old.tsx").await;
        tracker.record_module("/old", "/src/shared.tsx").await;
        tracker.record_module("/keep", "/src/keep.tsx").await;
        tracker.record_module("/keep", "/src/shared.tsx").await;

        // Mark /keep as active so it won't be evicted
        tracker.mark_active("/keep").await;

        // Add a third route to trigger eviction
        tracker.record_module("/new", "/src/new.tsx").await;

        let evicted = tracker.evict_lru().await;

        // One inactive route should be evicted (not /keep which is active)
        assert!(!evicted.is_empty(), "Should evict at least one route");
        for (route, _) in &evicted {
            assert_ne!(
                route.as_str(),
                "/keep",
                "Active route should not be evicted"
            );
        }

        // /src/shared.tsx should NOT be fully evicted — it's still used by /keep
        let routes_for_shared = tracker.routes_for_module("/src/shared.tsx").await;
        assert!(
            routes_for_shared.contains("/keep"),
            "shared.tsx should still be tracked for /keep"
        );

        // The evicted route's exclusive module should be fully removed
        for (_, exclusive_modules) in &evicted {
            for module in exclusive_modules {
                let routes = tracker.routes_for_module(module).await;
                assert!(
                    routes.is_empty(),
                    "Exclusive module {} should be fully evicted",
                    module
                );
            }
        }
    }
}
