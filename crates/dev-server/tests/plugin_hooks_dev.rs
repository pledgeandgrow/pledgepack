//! Dev-server module serving runs the same per-module plugin hooks
//! (`resolveId`/`load`/`transform`) as the production build, through the
//! shared `PluginHooks` trait.

use axum::http::{Request, StatusCode};
use pledgepack_core::config::{BuildMode, Framework, PledgeConfig};
use pledgepack_core::plugin_hooks::{BuildHook, CodeResult, HtmlResult, PluginHooks, ResolvedId};
use pledgepack_dev_server::PluginHookService;
use tower::ServiceExt;

/// Rewrites `__ANSWER__`, provides the virtual module `virtual:greeting`,
/// fails on `FAIL_ME`.
struct DevPlugin;

impl PluginHooks for DevPlugin {
    fn plugin_count(&self) -> usize {
        1
    }
    fn plugin_name(&self, _: usize) -> String {
        "dev-plugin".into()
    }
    fn supports(&self, _: usize, hook: BuildHook) -> bool {
        matches!(
            hook,
            BuildHook::ResolveId | BuildHook::Load | BuildHook::Transform
        )
    }
    fn resolve_id(
        &self,
        _: usize,
        source: &str,
        _: Option<&str>,
    ) -> anyhow::Result<Option<ResolvedId>> {
        Ok((source == "virtual:greeting").then(|| ResolvedId {
            id: "virtual:greeting".into(),
            external: false,
        }))
    }
    fn load(&self, _: usize, id: &str) -> anyhow::Result<Option<CodeResult>> {
        Ok((id == "virtual:greeting").then(|| CodeResult {
            code: "export const greeting = 'hello __ANSWER__';".into(),
            map: None,
        }))
    }
    fn transform(&self, _: usize, code: &str, id: &str) -> anyhow::Result<Option<CodeResult>> {
        if code.contains("FAIL_ME") {
            anyhow::bail!("refusing {id}");
        }
        Ok(code.contains("__ANSWER__").then(|| CodeResult {
            code: code.replace("__ANSWER__", "42"),
            map: None,
        }))
    }
    fn render_chunk(
        &self,
        _: usize,
        _: &str,
        _: &str,
        _: &str,
    ) -> anyhow::Result<Option<CodeResult>> {
        Ok(None)
    }
    fn transform_index_html(
        &self,
        _: usize,
        _: &str,
        _: &str,
    ) -> anyhow::Result<Option<HtmlResult>> {
        Ok(None)
    }
}

fn config(root: &std::path::Path) -> PledgeConfig {
    PledgeConfig {
        root: root.to_path_buf(),
        framework: Framework::Pledge,
        mode: BuildMode::Development,
        dev_server: pledgepack_core::config::DevServerConfig {
            hmr: false,
            ..Default::default()
        },
        cache: pledgepack_core::config::CacheConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, String) {
    let res = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).to_string())
}

fn app_with_plugin(root: &std::path::Path) -> axum::Router {
    let svc = PluginHookService::spawn(|| Some(Box::new(DevPlugin))).expect("service");
    pledgepack_dev_server::try_create_app_with_plugin_hooks(config(root), Some(svc)).unwrap()
}

#[tokio::test]
async fn dev_module_is_transformed_by_plugins_before_the_builtin_transform() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/main.ts"),
        "export const n: number = __ANSWER__;\n",
    )
    .unwrap();

    let (status, body) = get(app_with_plugin(tmp.path()), "/src/main.ts").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Plugin rewrote the source, then the built-in transform stripped the types.
    assert!(body.contains("= 42"), "{body}");
    assert!(!body.contains("__ANSWER__"), "{body}");
    assert!(!body.contains(": number"), "{body}");
}

#[tokio::test]
async fn without_plugin_hooks_the_module_is_served_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/main.ts"),
        "export const n: number = __ANSWER__;\n",
    )
    .unwrap();
    let app = pledgepack_dev_server::try_create_app(config(tmp.path())).unwrap();
    let (status, body) = get(app, "/src/main.ts").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("__ANSWER__"), "{body}");
}

#[tokio::test]
async fn plugin_virtual_modules_are_served_via_resolve_id_and_load() {
    let tmp = tempfile::tempdir().unwrap();
    let (status, body) = get(app_with_plugin(tmp.path()), "/@id/virtual:greeting").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // load() provided the code, then transform() ran over it.
    assert!(body.contains("hello 42"), "{body}");

    // Unknown ids still fall through to the normal (404) behaviour.
    let (status, _) = get(app_with_plugin(tmp.path()), "/@id/virtual:unknown").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn plugin_transform_error_becomes_an_error_module_naming_the_plugin() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/bad.ts"),
        "export const x = 'FAIL_ME';\n",
    )
    .unwrap();
    let (status, body) = get(app_with_plugin(tmp.path()), "/src/bad.ts").await;
    // Same convention as a failing built-in transform: a JS module that throws.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("dev-plugin"), "{body}");
    assert!(body.contains("transform"), "{body}");
}
