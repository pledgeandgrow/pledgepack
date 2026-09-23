//! End-to-end regression tests for bugs found by running the real CLI:
//! migrate hang, init, test runner (tests/ dir + relative imports), config
//! unknown-key detection, clean parse errors, missing-export errors,
//! `--root` fail-fast and positional `completions`.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// Run `pledge <args>` in `dir` with stdin closed and a hard timeout (a hang
/// fails the test instead of blocking CI).
fn pledge(dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pledge"))
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pledge");
    // Drain the pipes on reader threads while polling — a child whose output
    // exceeds the OS pipe buffer blocks on write and never exits, deadlocking
    // the try_wait loop (`pledge completions bash` emits a >64KB script).
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out_pipe, &mut v);
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err_pipe, &mut v);
        v
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("`pledge {}` hung (>90s)", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Output {
        status,
        stdout: out_t.join().unwrap(),
        stderr: err_t.join().unwrap(),
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

const VITE: &str = r#"import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
export default defineConfig({
  plugins: [react()],
  server: { port: 5199, open: true },
  resolve: { alias: { '@': '/src' } },
  build: { outDir: 'build', sourcemap: true, target: 'es2020' },
  define: { __APP__: '"x"' },
})
"#;

fn vite_project() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(r, "vite.config.ts", VITE);
    write(
        r,
        "package.json",
        r#"{"name":"p","dependencies":{"react":"18"}}"#,
    );
    write(
        r,
        "index.html",
        "<html><body><div id=root></div><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>",
    );
    write(r, "src/main.tsx", "export const x: number = 1;\n");
    t
}

#[test]
fn migrate_does_not_hang_and_maps_vite_settings() {
    let p = vite_project();
    let dry = pledge(p.path(), &["migrate", "--dry-run"]);
    let out = text(&dry);
    assert!(dry.status.success(), "{out}");
    assert!(out.contains("port: 5199"), "{out}");
    assert!(out.contains("outDir: 'build'"), "{out}");
    assert!(
        !p.path().join("pledge.config.ts").exists(),
        "dry run wrote a file"
    );

    let real = pledge(p.path(), &["migrate"]);
    assert!(real.status.success(), "{}", text(&real));
    let cfg = std::fs::read_to_string(p.path().join("pledge.config.ts")).unwrap();
    for needle in [
        "5199",
        "'build'",
        "es2020",
        "__APP__",
        "resolveAlias",
        "sourceMaps: true",
    ] {
        assert!(cfg.contains(needle), "missing {needle}:\n{cfg}");
    }
}

#[test]
fn init_carries_settings_adds_scripts_and_detects_typescript() {
    let p = vite_project();
    let o = pledge(p.path(), &["init"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("TypeScript:     yes"), "{out}");
    let cfg = std::fs::read_to_string(p.path().join("pledge.config.ts")).unwrap();
    assert!(cfg.contains("5199") && cfg.contains("'build'"), "{cfg}");
    assert!(p.path().join("tsconfig.json").exists());
    let pkg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(p.path().join("package.json")).unwrap())
            .unwrap();
    assert_eq!(pkg["scripts"]["dev"], "pledgepack dev");
    assert_eq!(pkg["scripts"]["build"], "pledgepack build");
}

fn test_project() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(
        r,
        "pledge.json",
        r#"{"entry":["src/index.ts"],"cache":{"enabled":false}}"#,
    );
    write(r, "src/index.ts", "export {};\n");
    write(
        r,
        "src/utils.ts",
        "export function greet(n: string): string { return 'hi ' + n; }\nexport default 7;\n",
    );
    write(
        r,
        "tests/utils.test.ts",
        "import { describe, it, expect } from 'vitest';\nimport def, { greet } from '../src/utils';\ndescribe('greet', () => {\n  it('greets', () => { expect(greet('bob')).toBe('hi bob'); expect(def).toBe(7); });\n});\n",
    );
    write(
        r,
        "src/x.test.ts",
        "import { greet } from './utils';\nit('works', () => { expect(greet('a')).toBe('hi a'); });\n",
    );
    t
}

#[test]
fn test_runner_finds_tests_dir_and_bundles_relative_imports() {
    let p = test_project();
    let o = pledge(p.path(), &["test"]);
    let out = text(&o);
    assert!(o.status.success(), "{out}");
    assert!(out.contains("Found 2 test file(s)"), "{out}");
    assert!(out.contains("2 passed"), "{out}");
    assert!(!out.contains("not defined"), "{out}");
}

#[test]
fn unknown_config_keys_warn_with_suggestion_and_fail_under_strict() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(
        r,
        "pledge.config.ts",
        "export default { entri: ['a'], bogusField: 1 };\n",
    );
    let o = pledge(r, &["config"]);
    let out = text(&o);
    assert!(
        out.contains("entri") && out.contains("Did you mean 'entry'"),
        "{out}"
    );
    assert!(out.contains("bogusField"), "{out}");
    assert!(!out.contains("no issues found"), "{out}");
    let s = pledge(r, &["--strict", "config"]);
    assert!(!s.status.success(), "{}", text(&s));
}

#[test]
fn build_parse_error_is_clean_and_fails() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(
        r,
        "pledge.json",
        r#"{"entry":["src/index.tsx"],"cache":{"enabled":false},"outDir":"dist"}"#,
    );
    write(r, "src/index.tsx", "export const a = 1;\nconst b = ;\n");
    let o = pledge(r, &["build"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(out.contains("error: "), "{out}");
    assert!(out.contains("--> src/index.tsx:2:"), "{out}");
    assert!(out.contains("const b = ;"), "{out}");
    for noise in ["OxcDiagnostic", "LabeledSpan", "Task computation failed"] {
        assert!(!out.contains(noise), "{noise} leaked:\n{out}");
    }
}

#[test]
fn build_rejects_import_of_missing_export() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(
        r,
        "pledge.json",
        r#"{"entry":["src/index.ts"],"cache":{"enabled":false},"outDir":"dist"}"#,
    );
    write(
        r,
        "src/index.ts",
        "import { zzz } from './utils';\nconsole.log(zzz);\n",
    );
    write(r, "src/utils.ts", "export const aaa = 1;\n");
    let o = pledge(r, &["build"]);
    let out = text(&o);
    assert!(!o.status.success(), "{out}");
    assert!(
        out.contains("\"zzz\" is not exported by \"src/utils.ts\""),
        "{out}"
    );
}

#[test]
fn nonexistent_root_fails_fast_and_completions_accepts_positional_shell() {
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("nope");
    let o = pledge(t.path(), &["--root", missing.to_str().unwrap(), "build"]);
    assert!(!o.status.success());
    assert!(
        text(&o).contains("Project root does not exist"),
        "{}",
        text(&o)
    );
    assert!(!missing.exists());

    let c = pledge(t.path(), &["completions", "bash"]);
    assert!(c.status.success(), "{}", text(&c));
    assert!(String::from_utf8_lossy(&c.stdout).contains("_pledge"));
    let c2 = pledge(t.path(), &["completions", "-s", "bash"]);
    assert!(c2.status.success());
}

fn http_get(port: u16, path: &str, accept_encoding: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nAccept: */*\r\nAccept-Encoding: {accept_encoding}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

#[test]
#[allow(clippy::zombie_processes)]
fn preview_spa_fallback_headers_and_asset_404s() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    write(r, "dist/index.html", "<html>INDEX</html>");
    write(r, "dist/app.js", "console.log(1)");
    write(r, "dist/app.js.map", "{}");
    write(r, "dist/.env", "SECRET=1");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pledge"))
        .current_dir(r)
        .args(["preview", "--out-dir", "dist", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("preview did not start");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let route = http_get(port, "/about", "gzip");
    let missing = http_get(port, "/missing.js", "gzip");
    let map = http_get(port, "/app.js.map", "gzip").to_lowercase();
    let env = http_get(port, "/.env", "gzip");
    let js = http_get(port, "/app.js", "gzip").to_lowercase();
    let _ = child.kill();
    assert!(
        route.starts_with("HTTP/1.1 200") && route.contains("INDEX"),
        "{route}"
    );
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    assert!(
        env.starts_with("HTTP/1.1 404") && !env.contains("SECRET"),
        "{env}"
    );
    assert!(map.contains("content-type: application/json"), "{map}");
    assert!(js.contains("vary: accept-encoding"), "{js}");
    assert!(js.contains("x-content-type-options: nosniff"), "{js}");
    assert!(js.contains("referrer-policy:"), "{js}");
}
