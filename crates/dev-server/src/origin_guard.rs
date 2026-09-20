//! Origin / Host validation for every dev-server endpoint.
//!
//! Without this, any web page open in the developer's browser can
//! (a) open a WebSocket to `ws://localhost:<port>/__pledge_hmr` (browsers do
//! not apply the same-origin policy to WebSocket handshakes), and
//! (b) use DNS rebinding to make `evil.example` resolve to 127.0.0.1 and then
//! read the dev server's responses same-origin.
//!
//! Policy:
//! * `Origin` (sent by browsers on WebSocket handshakes and cross-origin
//!   fetches) must be same-origin with the request's `Host`, a loopback alias
//!   on the same port, or listed in `PLEDGE_DEV_ALLOWED_ORIGINS`
//!   (comma-separated origins, e.g. `https://app.test`). A missing `Origin`
//!   (curl, native clients, plain navigations) is allowed. `dev_server.cors =
//!   "any"` relaxes the check for plain HTTP requests but never for the HMR
//!   WebSocket.
//! * When bound to a loopback address, `Host` must be a loopback name, the
//!   configured host, or listed in `PLEDGE_DEV_ALLOWED_HOSTS` (DNS-rebinding
//!   protection). Non-loopback binds are protected by the access token
//!   instead, since the legitimate `Host` can be any LAN name.

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub const ALLOWED_ORIGINS_ENV: &str = "PLEDGE_DEV_ALLOWED_ORIGINS";
pub const ALLOWED_HOSTS_ENV: &str = "PLEDGE_DEV_ALLOWED_HOSTS";

#[derive(Debug, Clone)]
pub struct OriginPolicy {
    bound_host: String,
    loopback_bind: bool,
    cors_any: bool,
    extra_origins: Vec<String>,
    extra_hosts: Vec<String>,
}

fn env_list(name: &str) -> Vec<String> {
    std::env::var(name)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().trim_end_matches('/').to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Split `host[:port]` (with optional `[v6]` brackets) into (host, port).
fn split_authority(authority: &str) -> (String, Option<String>) {
    let a = authority.trim().to_ascii_lowercase();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((h, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').map(str::to_string);
            return (format!("[{h}]"), port);
        }
        return (a, None);
    }
    match a.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), Some(p.into())),
        _ => (a, None),
    }
}

fn is_loopback_name(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h == "localhost"
        || h.ends_with(".localhost")
        || h.parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

impl OriginPolicy {
    pub fn new(bound_host: &str, cors_any: bool) -> Self {
        Self {
            bound_host: bound_host.to_ascii_lowercase(),
            loopback_bind: pledgepack_core::config::is_loopback_host(bound_host),
            cors_any,
            extra_origins: env_list(ALLOWED_ORIGINS_ENV),
            extra_hosts: env_list(ALLOWED_HOSTS_ENV),
        }
    }

    fn host_allowed(&self, host_header: &str) -> bool {
        if !self.loopback_bind {
            return true;
        }
        let (host, _) = split_authority(host_header);
        is_loopback_name(&host) || host == self.bound_host || self.extra_hosts.contains(&host)
    }

    fn origin_allowed(&self, origin: &str, host_header: Option<&str>) -> bool {
        let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
        if self.extra_origins.contains(&origin) {
            return true;
        }
        let Some((_scheme, authority)) = origin.split_once("://") else {
            return false; // includes the opaque `null` origin (sandboxed iframes, file://)
        };
        let Some(host_header) = host_header else {
            return false;
        };
        if authority == host_header.trim().to_ascii_lowercase() {
            return true;
        }
        // localhost <-> 127.0.0.1 <-> [::1] on the same port are the same server.
        let (oh, op) = split_authority(authority);
        let (hh, hp) = split_authority(host_header);
        is_loopback_name(&oh) && is_loopback_name(&hh) && op == hp
    }

    /// Validate request headers. `Err` carries a short reason.
    pub fn check(&self, headers: &HeaderMap, is_websocket: bool) -> Result<(), &'static str> {
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        if let Some(h) = host
            && !self.host_allowed(h)
        {
            return Err("Host header not allowed");
        }
        if let Some(origin) = headers.get(header::ORIGIN) {
            if self.cors_any && !is_websocket {
                return Ok(());
            }
            let ok = origin
                .to_str()
                .ok()
                .is_some_and(|o| self.origin_allowed(o, host));
            if !ok {
                return Err("Origin not allowed");
            }
        }
        Ok(())
    }
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Axum middleware body (use with `axum::middleware::from_fn`).
pub async fn enforce(
    policy: Arc<OriginPolicy>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let is_ws = is_websocket_upgrade(req.headers()) || req.uri().path() == "/__pledge_hmr";
    match policy.check(req.headers(), is_ws) {
        Ok(()) => next.run(req).await,
        Err(why) => {
            tracing::warn!("Rejected request to {}: {}", req.uri().path(), why);
            (StatusCode::FORBIDDEN, why).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn hm(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn loopback_policy() {
        let p = OriginPolicy::new("127.0.0.1", false);
        assert!(p.check(&hm(&[("host", "localhost:3000")]), false).is_ok());
        assert!(p.check(&hm(&[("host", "[::1]:3000")]), false).is_ok());
        assert!(p.check(&hm(&[("host", "evil.com")]), false).is_err());
        assert!(
            p.check(
                &hm(&[
                    ("host", "localhost:3000"),
                    ("origin", "http://localhost:3000")
                ]),
                true
            )
            .is_ok()
        );
        assert!(
            p.check(
                &hm(&[
                    ("host", "localhost:3000"),
                    ("origin", "http://127.0.0.1:3000")
                ]),
                true
            )
            .is_ok()
        );
        assert!(
            p.check(
                &hm(&[
                    ("host", "localhost:3000"),
                    ("origin", "http://localhost:4000")
                ]),
                true
            )
            .is_err()
        );
        assert!(
            p.check(&hm(&[("host", "localhost:3000"), ("origin", "null")]), true)
                .is_err()
        );
        assert!(
            p.check(
                &hm(&[("host", "localhost:3000"), ("origin", "http://evil.com")]),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn cors_any_never_relaxes_websockets() {
        let p = OriginPolicy::new("127.0.0.1", true);
        let h = hm(&[("host", "localhost:3000"), ("origin", "http://evil.com")]);
        assert!(p.check(&h, false).is_ok());
        assert!(p.check(&h, true).is_err());
    }

    #[test]
    fn non_loopback_bind_skips_host_check_but_not_origin() {
        let p = OriginPolicy::new("0.0.0.0", false);
        assert!(p.check(&hm(&[("host", "192.168.1.5:3000")]), false).is_ok());
        assert!(
            p.check(
                &hm(&[
                    ("host", "192.168.1.5:3000"),
                    ("origin", "http://192.168.1.5:3000")
                ]),
                true
            )
            .is_ok()
        );
        assert!(
            p.check(
                &hm(&[("host", "192.168.1.5:3000"), ("origin", "http://evil.com")]),
                true
            )
            .is_err()
        );
    }
}
