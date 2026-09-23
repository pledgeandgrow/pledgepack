//! Serving policy e2e: JSON/manifest handling against a real dev server.

use pledgepack_core::BuildEngine;
use pledgepack_core::config::{BuildMode, Framework, PledgeConfig};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn ensure_crypto_provider() {
    use std::sync::OnceLock;
    static PROVIDER: OnceLock<()> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

async fn start(root: &Path) -> (u16, tokio::task::JoinHandle<()>) {
    ensure_crypto_provider();
    let port = find_free_port();
    let cfg = PledgeConfig {
        root: root.to_path_buf(),
        framework: Framework::Pledge,
        mode: BuildMode::Development,
        dev_server: pledgepack_core::config::DevServerConfig {
            port,
            host: "127.0.0.1".to_string(),
            hmr: false,
            ..Default::default()
        },
        cache: pledgepack_core::config::CacheConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let engine = BuildEngine::new(Arc::new(cfg.clone()));
    let handle = tokio::spawn(async move {
        let _ = pledgepack_dev_server::serve(engine, &cfg).await;
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (port, handle)
}

fn ctype(r: &reqwest::Response) -> String {
    r.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn json_is_data_unless_imported_and_manifests_are_denied() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("package.json"), r#"{"name":"secret-app"}"#).unwrap();
    std::fs::write(
        tmp.path().join("package-lock.json"),
        r#"{"lockfileVersion":3}"#,
    )
    .unwrap();
    std::fs::write(tmp.path().join("src/data.json"), r#"{"a":1}"#).unwrap();
    std::fs::write(
        tmp.path().join("src/index.tsx"),
        "import data from './data.json';\nexport default data;\n",
    )
    .unwrap();

    let (port, server) = start(tmp.path()).await;
    let c = reqwest::Client::new();
    let get = |p: &str| c.get(format!("http://127.0.0.1:{port}{p}")).send();

    // package.json / lockfiles: refused to plain fetches.
    assert_eq!(get("/package.json").await.unwrap().status(), 403);
    assert_eq!(get("/package-lock.json").await.unwrap().status(), 403);

    // ...and never served as application/javascript.
    let pj = get("/package.json").await.unwrap();
    assert!(!ctype(&pj).contains("javascript"));

    // A JSON file fetched as data is raw JSON.
    let raw = get("/src/data.json").await.unwrap();
    assert_eq!(raw.status(), 200);
    assert!(ctype(&raw).contains("application/json"), "{}", ctype(&raw));
    assert_eq!(raw.text().await.unwrap(), r#"{"a":1}"#);

    // Imported as a module (?import): wrapped as JS.
    let module = get("/src/data.json?import").await.unwrap();
    assert_eq!(module.status(), 200);
    assert!(ctype(&module).contains("javascript"), "{}", ctype(&module));
    assert!(module.text().await.unwrap().contains("export"));

    // Browsers importing a module send Sec-Fetch-Dest: script.
    let script = c
        .get(format!("http://127.0.0.1:{port}/src/data.json"))
        .header("Sec-Fetch-Dest", "script")
        .send()
        .await
        .unwrap();
    assert!(ctype(&script).contains("javascript"));

    // package.json may still be imported as a module.
    let pj_mod = get("/package.json?import").await.unwrap();
    assert_eq!(pj_mod.status(), 200);
    assert!(ctype(&pj_mod).contains("javascript"));
    // ...but lockfiles never.
    assert_eq!(
        get("/package-lock.json?import").await.unwrap().status(),
        403
    );

    // The importing module's specifier is marked so the browser gets the module.
    let idx = get("/src/index.tsx").await.unwrap().text().await.unwrap();
    assert!(idx.contains("data.json?import"), "index.tsx was: {idx}");

    server.abort();
}
