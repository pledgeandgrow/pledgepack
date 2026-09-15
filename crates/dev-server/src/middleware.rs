// Dev server middleware chain (feature 14: configurable middleware pipeline)
//
// Provides a configurable middleware pipeline that processes requests before
// they reach the module handler. Middleware functions can modify request
// headers, rewrite URLs, add CORS headers, inject scripts, or short-circuit
// responses.

use std::collections::HashMap;

/// A middleware function that processes a request before it reaches the handler.
/// Middleware functions are identified by a name and can be configured via the
/// dev server config.
pub struct MiddlewareFn {
    /// Name identifying the middleware (e.g., "cors", "rewrite", "headers")
    pub name: String,
    /// Source code of the middleware (for logging/debugging)
    pub source: String,
    /// The kind of middleware
    pub kind: MiddlewareKind,
}

/// Supported middleware types
#[derive(Debug, Clone)]
pub enum MiddlewareKind {
    /// Add CORS headers to responses
    Cors {
        origin: String,
        methods: Vec<String>,
        headers: Vec<String>,
    },
    /// Rewrite URL paths
    Rewrite { from: String, to: String },
    /// Add custom headers to responses
    Headers { headers: HashMap<String, String> },
    /// Proxy requests to another server
    Proxy { target: String, path_prefix: String },
    /// Custom middleware (logged but not executed natively)
    Custom { source: String },
}

impl MiddlewareFn {
    /// Parse a middleware function from a source string.
    /// The source can be a JSON config or a simple directive.
    pub fn from_source(source: &str) -> Option<Self> {
        let trimmed = source.trim();

        // Try parsing as JSON
        if trimmed.starts_with('{')
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return Self::from_json(&json);
        }

        // Try parsing as a simple directive: "cors", "cors:origin=*", etc.
        if let Some((name, args)) = trimmed.split_once(':') {
            let name = name.trim().to_lowercase();
            let args = args.trim();
            return Self::from_directive(&name, args);
        }

        // Single keyword middleware
        match trimmed.to_lowercase().as_str() {
            "cors" => Some(Self {
                name: "cors".into(),
                source: source.into(),
                kind: MiddlewareKind::Cors {
                    origin: "*".into(),
                    methods: vec![
                        "GET".into(),
                        "POST".into(),
                        "PUT".into(),
                        "DELETE".into(),
                        "PATCH".into(),
                        "OPTIONS".into(),
                    ],
                    headers: vec!["Content-Type".into(), "Authorization".into()],
                },
            }),
            _ => None,
        }
    }

    fn from_json(json: &serde_json::Value) -> Option<Self> {
        let name = json.get("name").and_then(|v| v.as_str())?;
        let name = name.to_string();

        let kind = match name.as_str() {
            "cors" => {
                let origin = json
                    .get("origin")
                    .and_then(|v| v.as_str())
                    .unwrap_or("*")
                    .to_string();
                let methods = json
                    .get("methods")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["GET".into(), "POST".into(), "OPTIONS".into()]);
                let headers = json
                    .get("headers")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["Content-Type".into()]);
                MiddlewareKind::Cors {
                    origin,
                    methods,
                    headers,
                }
            }
            "rewrite" => {
                let from = json.get("from").and_then(|v| v.as_str())?.to_string();
                let to = json.get("to").and_then(|v| v.as_str())?.to_string();
                MiddlewareKind::Rewrite { from, to }
            }
            "headers" => {
                let mut headers = HashMap::new();
                if let Some(obj) = json.get("headers").and_then(|v| v.as_object()) {
                    for (key, val) in obj {
                        if let Some(s) = val.as_str() {
                            headers.insert(key.clone(), s.to_string());
                        }
                    }
                }
                MiddlewareKind::Headers { headers }
            }
            "proxy" => {
                let target = json.get("target").and_then(|v| v.as_str())?.to_string();
                let path_prefix = json
                    .get("pathPrefix")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                MiddlewareKind::Proxy {
                    target,
                    path_prefix,
                }
            }
            _ => MiddlewareKind::Custom {
                source: json.to_string(),
            },
        };

        Some(Self {
            name,
            source: json.to_string(),
            kind,
        })
    }

    fn from_directive(name: &str, args: &str) -> Option<Self> {
        match name {
            "cors" => {
                let origin = if args.contains("origin=") {
                    args.split("origin=")
                        .nth(1)
                        .and_then(|s| s.split('&').next())
                        .unwrap_or("*")
                        .to_string()
                } else {
                    "*".to_string()
                };
                Some(Self {
                    name: "cors".into(),
                    source: format!("cors:{}", args),
                    kind: MiddlewareKind::Cors {
                        origin,
                        methods: vec![
                            "GET".into(),
                            "POST".into(),
                            "PUT".into(),
                            "DELETE".into(),
                            "PATCH".into(),
                            "OPTIONS".into(),
                        ],
                        headers: vec!["Content-Type".into(), "Authorization".into()],
                    },
                })
            }
            "rewrite" => {
                let parts: Vec<&str> = args.splitn(2, "->").collect();
                if parts.len() == 2 {
                    Some(Self {
                        name: "rewrite".into(),
                        source: format!("rewrite:{}", args),
                        kind: MiddlewareKind::Rewrite {
                            from: parts[0].trim().to_string(),
                            to: parts[1].trim().to_string(),
                        },
                    })
                } else {
                    None
                }
            }
            "headers" => {
                let mut headers = HashMap::new();
                for pair in args.split(',') {
                    if let Some((key, val)) = pair.split_once('=') {
                        headers.insert(key.trim().to_string(), val.trim().to_string());
                    }
                }
                if headers.is_empty() {
                    None
                } else {
                    Some(Self {
                        name: "headers".into(),
                        source: format!("headers:{}", args),
                        kind: MiddlewareKind::Headers { headers },
                    })
                }
            }
            _ => Some(Self {
                name: name.to_string(),
                source: format!("{}:{}", name, args),
                kind: MiddlewareKind::Custom {
                    source: args.to_string(),
                },
            }),
        }
    }
}

// PRODUCTION-READINESS-100.md goal 91: `apply_cors_headers` and
// `apply_rewrite` were removed from here — both were confirmed genuinely
// unused (zero call sites anywhere in this repo or in pledgejs) rather than
// reserved for a specific planned feature. `apply_cors_headers` in
// particular was a hand-rolled, never-wired CORS implementation that had
// been superseded by the real one: `DevServerConfig.cors` +
// `tower_http::cors::CorsLayer` in `crates/dev-server/src/lib.rs`'s
// `serve()` (see Phase 1 goal 16) — keeping both around risked a future
// reader mistaking the dead one for the active implementation.
