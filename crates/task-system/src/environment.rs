// Environment — the build target context for a task.
//
// In Turbopack, `Environment` is a first-class concept that's part of the task
// identity. The same source file parsed in a `Client` vs `Server` environment
// produces different task nodes, but shared dependencies (like reading a file)
// produce the same task node regardless of environment.
//
// PledgePack adopts this design:
//   - `Environment` is part of the `TaskId`: `TaskId = blake3(fn_id ++ inputs ++ env)`
//   - Environment-agnostic tasks (e.g., `read_file`) use `Environment::Shared`
//     so they're computed once and shared across environments.
//   - Environment inheritance: a task inherits its caller's environment unless
//     explicitly overridden via `with_environment()`.
//   - Cross-environment dependencies: a server task can depend on a client
//     task's output — this is just a normal dependency edge with different
//     environment-qualified TaskIds.
//
// # Thread-Local Environment Context
//
// During task execution, the current environment is stored in a thread-local.
// When a task calls another task (e.g., `parse_module` calls `read_file`),
// the called task inherits the caller's environment unless it explicitly
// overrides it. This mirrors Turbopack's `Environment` propagation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// G5.9: Global registry for custom environment names.
///
/// Maps a u64 hash of the custom environment name to the name string.
/// This allows plugins to define arbitrary environments while keeping
/// `Environment` as a `Copy` type.
static CUSTOM_ENV_REGISTRY: OnceLock<RwLock<HashMap<u64, String>>> = OnceLock::new();

fn registry() -> &'static RwLock<HashMap<u64, String>> {
    CUSTOM_ENV_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The build target environment for a task.
///
/// This is mixed into the `TaskId` hash so that the same source file
/// produces different task nodes in different environments.
///
/// `Shared` is used for environment-agnostic tasks (e.g., reading a file
/// from disk) that should be computed once and shared across all environments.
///
/// G5.9: `Custom` allows plugins to define arbitrary environments (e.g.,
/// "rust-wasm", "deno", "bun"). The u64 is a hash of the environment name,
/// looked up in a global registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Environment {
    /// Browser/client-side code. JSX transforms to DOM operations.
    /// Target: ES2020+, ESM output, browser-compatible APIs.
    Client,
    /// Server-side code (Node.js, Bun, Deno). JSX transforms to string/SSR.
    /// Target: Node 18+, CJS or ESM output, Node APIs available.
    Server,
    /// Edge runtime (Cloudflare Workers, Vercel Edge, Deno Deploy).
    /// Target: minimal APIs, no Node built-ins, Web APIs only.
    Edge,
    /// Web Worker context. No DOM access, limited APIs.
    /// Target: ES2020+, ESM, Worker-compatible APIs.
    Worker,
    /// Environment-agnostic — the task's output is the same regardless of
    /// environment. Used for file I/O, config parsing, etc.
    /// Tasks with this environment share a single task node across all
    /// environments, avoiding redundant computation.
    Shared,
    /// G5.9: A plugin-defined custom environment.
    /// The u64 is a blake3 hash of the environment name, registered via
    /// `Environment::register_custom()`.
    Custom(u64),
}

impl Environment {
    /// Whether this environment is environment-specific (not `Shared`).
    pub fn is_specific(&self) -> bool {
        !matches!(self, Environment::Shared)
    }

    /// Whether this environment is the client.
    pub fn is_client(&self) -> bool {
        matches!(self, Environment::Client)
    }

    /// Whether this environment is the server.
    pub fn is_server(&self) -> bool {
        matches!(self, Environment::Server)
    }

    /// Whether this environment is the edge.
    pub fn is_edge(&self) -> bool {
        matches!(self, Environment::Edge)
    }

    /// Whether this environment is a worker.
    pub fn is_worker(&self) -> bool {
        matches!(self, Environment::Worker)
    }

    /// A stable string representation for hashing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Environment::Client => "client",
            Environment::Server => "server",
            Environment::Edge => "edge",
            Environment::Worker => "worker",
            Environment::Shared => "shared",
            Environment::Custom(id) => {
                // Look up the registered name. Leak the string to make it
                // 'static so we can keep the &'static str return type (the
                // registry stores owned Strings). Falls back to "custom" if
                // the environment was never registered.
                let registry = registry().read().unwrap();
                match registry.get(id) {
                    Some(name) => Box::leak(name.clone().into_boxed_str()) as &'static str,
                    None => "custom",
                }
            }
        }
    }

    /// G5.9: Get the name of a custom environment, or the standard name for built-in environments.
    pub fn name(&self) -> String {
        match self {
            Environment::Custom(id) => registry()
                .read()
                .unwrap()
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("custom-{}", id)),
            other => other.as_str().to_string(),
        }
    }

    /// G5.9: Register a custom environment by name.
    ///
    /// Returns an `Environment::Custom(hash)` that can be used as a task
    /// environment. The name is stored in a global registry for lookup.
    /// Registering the same name twice returns the same `Environment`.
    pub fn register_custom(name: &str) -> Environment {
        let hash = blake3::hash(name.as_bytes());
        let id = u64::from_be_bytes(hash.as_bytes()[..8].try_into().unwrap());
        let mut registry = registry().write().unwrap();
        if let Some(existing) = registry.get(&id) {
            if existing != name {
                tracing::error!(
                    "Environment hash collision: '{}' and '{}' both hash to {}",
                    existing,
                    name,
                    id
                );
                // Still use the existing entry to avoid corruption
            }
        } else {
            registry.insert(id, name.to_string());
        }
        Environment::Custom(id)
    }

    /// G5.9: Create a custom environment from a name without registering.
    ///
    /// This is useful for comparing against registered environments.
    pub fn custom(name: &str) -> Environment {
        let hash = blake3::hash(name.as_bytes());
        let id = u64::from_be_bytes(hash.as_bytes()[..8].try_into().unwrap());
        Environment::Custom(id)
    }

    /// G5.9: Whether this is a custom environment.
    pub fn is_custom(&self) -> bool {
        matches!(self, Environment::Custom(_))
    }

    /// All environment variants (excluding `Shared`).
    pub fn all_specific() -> &'static [Environment] {
        &[
            Environment::Client,
            Environment::Server,
            Environment::Edge,
            Environment::Worker,
        ]
    }
}

impl Default for Environment {
    fn default() -> Self {
        Environment::Shared
    }
}

impl std::fmt::Display for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// --- Thread-Local Environment Context ---

thread_local! {
    /// The current environment for the executing task on this thread.
    /// Set by `TaskEngine::compute_task()` before calling the executor.
    /// Read by `current_environment()` when tasks need to know their env.
    static CURRENT_ENVIRONMENT: RefCell<Environment> = RefCell::new(Environment::Shared);
}

/// Get the current environment for the executing task on this thread.
///
/// Returns `Environment::Shared` if no task is currently executing
/// (e.g., when called from outside the task engine).
pub fn current_environment() -> Environment {
    CURRENT_ENVIRONMENT.with(|env| *env.borrow())
}

/// Set the current environment for this thread.
///
/// Called by `TaskEngine::compute_task()` before executing a task.
/// The previous environment is restored after the task completes.
pub(crate) fn with_environment<F, R>(env: Environment, f: F) -> R
where
    F: FnOnce() -> R,
{
    let prev = CURRENT_ENVIRONMENT.with(|cell| {
        let prev = *cell.borrow();
        *cell.borrow_mut() = env;
        prev
    });
    let result = f();
    CURRENT_ENVIRONMENT.with(|cell| {
        *cell.borrow_mut() = prev;
    });
    result
}

/// Run a closure with a specific environment, returning the result.
///
/// This is the public API for tasks that need to explicitly set their
/// environment (e.g., a server component rendering its client counterpart).
pub fn run_with_environment<F, R>(env: Environment, f: F) -> R
where
    F: FnOnce() -> R,
{
    with_environment(env, f)
}

// ─── G5.12: Custom Environment Plugins ─────────────────────────────────

/// A custom environment plugin that extends the environment system with
/// runtime-specific behavior (e.g., Deno, Bun, Cloudflare Workers).
///
/// Custom environment plugins can register file extensions, aliases, and
/// runtime-specific transforms. This allows PledgePack to target non-standard
/// runtimes without hardcoding them.
#[derive(Clone, Debug)]
pub struct EnvironmentPlugin {
    /// The plugin name (e.g., "deno", "bun", "workerd").
    pub name: String,
    /// File extensions this plugin handles (e.g., [".ts", ".tsx"]).
    pub extensions: Vec<String>,
    /// Aliases for this environment (e.g., ["cloudflare"] for workerd).
    pub aliases: Vec<String>,
    /// Whether this plugin is enabled.
    pub enabled: bool,
}

impl EnvironmentPlugin {
    /// Create a new environment plugin with the given name.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            extensions: Vec::new(),
            aliases: Vec::new(),
            enabled: true,
        }
    }

    /// Add a file extension this plugin handles.
    pub fn with_extension(mut self, ext: &str) -> Self {
        self.extensions.push(ext.to_string());
        self
    }

    /// Add an alias for this environment.
    pub fn with_alias(mut self, alias: &str) -> Self {
        self.aliases.push(alias.to_string());
        self
    }

    /// Check if this plugin handles a given file extension.
    pub fn matches_extension(&self, ext: &str) -> bool {
        self.extensions.iter().any(|e| e == ext)
    }

    /// Check if this plugin matches a given alias.
    pub fn matches_alias(&self, alias: &str) -> bool {
        self.aliases.iter().any(|a| a == alias)
    }
}

/// Registry of custom environment plugins.
pub struct EnvironmentPluginRegistry {
    plugins: Vec<EnvironmentPlugin>,
}

impl EnvironmentPluginRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Register a custom environment plugin.
    pub fn register(&mut self, plugin: EnvironmentPlugin) {
        self.plugins.push(plugin);
    }

    /// Find a plugin that handles a given file extension.
    pub fn find_for_extension(&self, ext: &str) -> Option<&EnvironmentPlugin> {
        self.plugins.iter().find(|p| p.matches_extension(ext))
    }

    /// Find a plugin by alias.
    pub fn find_by_alias(&self, alias: &str) -> Option<&EnvironmentPlugin> {
        self.plugins.iter().find(|p| p.matches_alias(alias))
    }

    /// Number of registered plugins.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl Default for EnvironmentPluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_is_specific() {
        assert!(Environment::Client.is_specific());
        assert!(Environment::Server.is_specific());
        assert!(Environment::Edge.is_specific());
        assert!(Environment::Worker.is_specific());
        assert!(!Environment::Shared.is_specific());
    }

    #[test]
    fn environment_as_str() {
        assert_eq!(Environment::Client.as_str(), "client");
        assert_eq!(Environment::Server.as_str(), "server");
        assert_eq!(Environment::Edge.as_str(), "edge");
        assert_eq!(Environment::Worker.as_str(), "worker");
        assert_eq!(Environment::Shared.as_str(), "shared");
    }

    #[test]
    fn thread_local_environment_defaults_to_shared() {
        assert_eq!(current_environment(), Environment::Shared);
    }

    #[test]
    fn with_environment_sets_and_restores() {
        assert_eq!(current_environment(), Environment::Shared);

        let result = with_environment(Environment::Client, || {
            assert_eq!(current_environment(), Environment::Client);
            42
        });

        assert_eq!(result, 42);
        assert_eq!(current_environment(), Environment::Shared);
    }

    #[test]
    fn nested_environments_restore_correctly() {
        let r1 = with_environment(Environment::Server, || {
            assert_eq!(current_environment(), Environment::Server);
            let r2 = with_environment(Environment::Client, || {
                assert_eq!(current_environment(), Environment::Client);
                100
            });
            assert_eq!(current_environment(), Environment::Server);
            r2
        });
        assert_eq!(current_environment(), Environment::Shared);
        assert_eq!(r1, 100);
    }

    #[test]
    fn run_with_environment_is_public() {
        let result = run_with_environment(Environment::Edge, || {
            assert_eq!(current_environment(), Environment::Edge);
            "edge-result"
        });
        assert_eq!(result, "edge-result");
        assert_eq!(current_environment(), Environment::Shared);
    }

    #[test]
    fn all_specific_excludes_shared() {
        let envs = Environment::all_specific();
        assert_eq!(envs.len(), 4);
        assert!(!envs.contains(&Environment::Shared));
    }

    // ─── G5.9: Custom environments via plugins tests ────────────────

    #[test]
    fn register_custom_returns_consistent_id() {
        let env1 = Environment::register_custom("rust-wasm");
        let env2 = Environment::register_custom("rust-wasm");
        assert_eq!(env1, env2, "Same name should produce same Environment");
    }

    #[test]
    fn register_custom_different_names_differ() {
        let env1 = Environment::register_custom("deno");
        let env2 = Environment::register_custom("bun");
        assert_ne!(
            env1, env2,
            "Different names should produce different Environments"
        );
    }

    #[test]
    fn custom_environment_is_specific() {
        let env = Environment::register_custom("my-runtime");
        assert!(env.is_specific(), "Custom environment should be specific");
        assert!(!env.is_client(), "Custom should not be client");
        assert!(!env.is_server(), "Custom should not be server");
    }

    #[test]
    fn custom_environment_is_custom() {
        let env = Environment::register_custom("test-env");
        assert!(env.is_custom(), "Custom environment should be detected");
        assert!(
            !Environment::Client.is_custom(),
            "Built-in should not be custom"
        );
    }

    #[test]
    fn custom_environment_name_lookup() {
        let env = Environment::register_custom("my-custom-env");
        assert_eq!(
            env.name(),
            "my-custom-env",
            "Registered name should be looked up"
        );
    }

    #[test]
    fn custom_environment_name_fallback() {
        let env = Environment::custom("unregistered-env");
        // Not registered, so name falls back to custom-{id}
        assert!(
            env.name().starts_with("custom-"),
            "Unregistered should fall back"
        );
    }

    #[test]
    fn custom_environment_differs_from_builtins() {
        let env = Environment::register_custom("client");
        assert_ne!(
            env,
            Environment::Client,
            "Custom 'client' should differ from builtin Client"
        );
        assert!(
            env.is_custom(),
            "Should be custom even if name matches builtin"
        );
    }

    #[test]
    fn custom_environment_with_thread_local() {
        let env = Environment::register_custom("test-runtime");
        let result = run_with_environment(env, || {
            assert_eq!(current_environment(), env);
            assert!(current_environment().is_custom());
            42
        });
        assert_eq!(result, 42);
        assert_eq!(current_environment(), Environment::Shared);
    }

    // ─── G5.12: Custom Environment Plugins tests ─────────────────────

    #[test]
    fn g5_12_environment_plugin_registration() {
        let plugin = EnvironmentPlugin::new("deno")
            .with_extension(".ts")
            .with_extension(".tsx")
            .with_alias("deno-server");
        assert_eq!(plugin.name, "deno");
        assert_eq!(plugin.extensions.len(), 2);
        assert_eq!(plugin.aliases.len(), 1);
    }

    #[test]
    fn g5_12_environment_plugin_matches_extension() {
        let plugin = EnvironmentPlugin::new("bun")
            .with_extension(".js")
            .with_extension(".jsx");
        assert!(plugin.matches_extension(".js"));
        assert!(plugin.matches_extension(".jsx"));
        assert!(!plugin.matches_extension(".ts"));
    }

    #[test]
    fn g5_12_environment_plugin_matches_alias() {
        let plugin = EnvironmentPlugin::new("workerd").with_alias("cloudflare");
        assert!(plugin.matches_alias("cloudflare"));
        assert!(!plugin.matches_alias("deno"));
    }

    #[test]
    fn g5_12_environment_plugin_registry() {
        let mut registry = EnvironmentPluginRegistry::new();
        registry.register(EnvironmentPlugin::new("deno").with_extension(".ts"));
        registry.register(EnvironmentPlugin::new("bun").with_extension(".js"));

        assert_eq!(registry.len(), 2);
        let matched = registry.find_for_extension(".ts");
        assert!(matched.is_some());
        assert_eq!(matched.unwrap().name, "deno");
    }
}
