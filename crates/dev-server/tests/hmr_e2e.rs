//! End-to-end HMR: real dev server, real file watcher, real WebSocket client.
//!
//! Edits a source file on disk and expects the server to push an `update`
//! message for that module over `/__pledge_hmr`.

use futures_util::StreamExt;
use pledgepack_core::BuildEngine;
use pledgepack_core::config::{BuildMode, Framework, PledgeConfig};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

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

fn config(root: &Path, port: u16, host: &str) -> PledgeConfig {
    PledgeConfig {
        root: root.to_path_buf(),
        framework: Framework::Pledge,
        mode: BuildMode::Development,
        dev_server: pledgepack_core::config::DevServerConfig {
            port,
            host: host.to_string(),
            hmr: true,
            ..Default::default()
        },
        cache: pledgepack_core::config::CacheConfig {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn start_server(root: &Path, host: &str) -> (u16, tokio::task::JoinHandle<()>) {
    ensure_crypto_provider();
    let port = find_free_port();
    let cfg = config(root, port, host);
    let engine = BuildEngine::new(Arc::new(cfg.clone()));
    let handle = tokio::spawn(async move {
        let _ = pledgepack_dev_server::serve(engine, &cfg).await;
    });
    // Wait until the port accepts connections.
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

/// Read WS messages until one parses as JSON with `type == wanted`.
async fn next_of_type(
    ws: &mut (
             impl futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
    wanted: &str,
    within: Duration,
) -> Option<serde_json::Value> {
    let deadline = Instant::now() + within;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let msg = match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => return None,
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            _ => continue,
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && v["type"] == wanted
        {
            return Some(v);
        }
    }
    None
}

/// Connect, wait for `connected`, then edit `file` repeatedly until an
/// `update` for `expected_path` arrives.
async fn assert_edit_produces_update(port: u16, file: &Path, expected_path: &str) {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/__pledge_hmr"))
            .await
            .expect("ws connect");
    assert!(
        next_of_type(&mut ws, "connected", Duration::from_secs(5))
            .await
            .is_some(),
        "no `connected` hello"
    );

    // Let the watcher thread arm, then edit (retrying: the first edit can race
    // the watcher start-up, later ones must not).
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut n = 0;
    let mut update = None;
    while Instant::now() < deadline && update.is_none() {
        n += 1;
        let mut content = std::fs::read_to_string(file).unwrap();
        content.push_str(&format!("\nexport const edit{n} = {n};\n"));
        std::fs::write(file, content).unwrap();
        update = next_of_type(&mut ws, "update", Duration::from_secs(2)).await;
    }
    let update = update.expect("no `update` HMR message after editing the file");
    assert_eq!(update["path"], expected_path);
    assert!(
        update["full_code"]
            .as_str()
            .is_some_and(|c| c.contains("export const edit")),
        "update should carry the new file content: {update}"
    );
}

#[tokio::test]
async fn editing_a_source_file_pushes_an_hmr_update_to_a_ws_client() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file = src.join("index.tsx");
    std::fs::write(&file, "export const a = 1;\n").unwrap();

    let (port, server) = start_server(tmp.path(), "127.0.0.1").await;
    assert_edit_produces_update(port, &file, "src/index.tsx").await;
    server.abort();
}

#[tokio::test]
async fn hmr_works_when_project_lives_under_an_ignored_named_directory() {
    // Regression: a project below e.g. `.../target/...` got zero watcher events.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("target").join("dist").join("proj");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let file = root.join("src/index.tsx");
    std::fs::write(&file, "export const a = 1;\n").unwrap();

    let (port, server) = start_server(&root, "127.0.0.1").await;
    assert_edit_produces_update(port, &file, "src/index.tsx").await;
    server.abort();
}

#[tokio::test]
async fn localhost_binds_both_loopback_families() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/index.tsx"), "export {};\n").unwrap();

    let (port, server) = start_server(tmp.path(), "localhost").await;
    let client = reqwest::Client::new();
    let v4 = client
        .get(format!("http://127.0.0.1:{port}/src/index.tsx"))
        .send()
        .await
        .expect("http://127.0.0.1 must be reachable when host = localhost");
    assert!(v4.status().is_success(), "v4 status {}", v4.status());

    // Only assert IPv6 where the OS supports it.
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        let v6 = client
            .get(format!("http://[::1]:{port}/src/index.tsx"))
            .send()
            .await
            .expect("http://[::1] must be reachable when host = localhost");
        assert!(v6.status().is_success(), "v6 status {}", v6.status());
    }
    server.abort();
}
