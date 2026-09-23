//! End-to-end coverage that actually *executes* `pledge build` output.
//!
//! Every pre-existing fixture was a single-module entry, which is why the
//! old "concatenate transformed sources" emit shipped broken output for
//! months: dangling `import "./util"` specifiers were never noticed. These
//! fixtures are multi-module, build through the real CLI binary, and run
//! the emitted chunks under Node with a DOM shim — asserting observable
//! behavior, not just file presence.
//!
//! Tests skip (pass) when `node` is not on PATH.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn pledge(dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pledge"))
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pledge");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("`pledge {}` hung (>120s)", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Minimal Node harness: DOM shim, manifest eval, chunk-URL rewrite to
/// paths relative to the output dir, then eval every non-async chunk in
/// index.html order and the entry last. `__pp.dyn` uses `import(f)`, which
/// Node resolves relative to this harness file (it lives in out_dir).
const HARNESS: &str = r#"
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
const dir = process.argv[2];
const entry = process.argv[3];
globalThis.document = {
  getElementById: () => ({ set textContent(v) { console.log('DOM:', v); } }),
  querySelector: () => null,
};
try { eval(readFileSync(join(dir, '__pp_manifest.js'), 'utf8')); } catch {}
if (globalThis.__pp && __pp.chunks) {
  for (const k of Object.keys(__pp.chunks)) {
    __pp.chunks[k] = './' + __pp.chunks[k].split('/').pop();
  }
}
// Load every non-async chunk first (vendor/shared), mirroring the HTML
// script order — async chunks stay lazy via __pp.dyn.
const sync = readdirSync(dir).filter(f =>
  f.endsWith('.js') && !f.startsWith('__pp') && !f.startsWith('_') &&
  !f.includes('async') && f !== entry
);
for (const f of sync) eval(readFileSync(join(dir, f), 'utf8'));
eval(readFileSync(join(dir, entry), 'utf8'));
await new Promise(r => setTimeout(r, 400));
console.log('DONE');
"#;

/// Build `dir`, then execute the emitted entry chunk under Node and return
/// its stdout. Asserts the build itself succeeded.
fn build_and_run(dir: &Path, entry_name: &str) -> String {
    let out = pledge(dir, &["build"]);
    assert!(out.status.success(), "pledge build failed:\n{}", text(&out));

    let out_dir = dir.join(".pledge");
    let entry_js = std::fs::read_dir(&out_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.starts_with(entry_name) && n.ends_with(".js"))
        .unwrap_or_else(|| panic!("no {entry_name}*.js emitted in {}", out_dir.display()));

    let harness = out_dir.join("_run.mjs");
    std::fs::write(&harness, HARNESS).unwrap();
    let run = Command::new("node")
        .arg(&harness)
        .arg(&out_dir)
        .arg(&entry_js)
        .current_dir(&out_dir)
        .output()
        .expect("spawn node");
    let run_text = text(&run);
    assert!(
        run.status.success(),
        "emitted bundle failed under node:\n{run_text}"
    );
    run_text
}

/// Base project scaffold shared by every fixture.
fn scaffold(dir: &Path) {
    write(
        dir,
        "pledge.config.ts",
        r#"export default {
  entry: ['src/index.ts'],
  outDir: '.pledge',
};
"#,
    );
    write(
        dir,
        "index.html",
        r#"<!DOCTYPE html><html><body><div id="root"></div>
<script src="/src/index.ts"></script></body></html>"#,
    );
}

#[test]
fn emitted_bundle_executes_vanilla_multimodule() {
    if !node_available() {
        eprintln!("node not on PATH — skipping");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(
        t.path(),
        "src/index.ts",
        r#"import { greet } from "./util";
import "./style.css";
const app = document.getElementById("root");
if (app) app.textContent = greet("pledgepack");
import("./lazy").then((m) => m.lazyHello());
"#,
    );
    write(
        t.path(),
        "src/util.ts",
        r#"export function greet(name: string): string {
    return `Hello, ${name}!`;
}
"#,
    );
    write(
        t.path(),
        "src/lazy.ts",
        r#"export function lazyHello() {
    console.log("lazy chunk loaded");
}
"#,
    );
    write(t.path(), "src/style.css", "body { margin: 0; }\n");

    let out = build_and_run(t.path(), "entry-");
    assert!(out.contains("DOM: Hello, pledgepack!"), "got:\n{out}");
    assert!(out.contains("lazy chunk loaded"), "got:\n{out}");
    assert!(out.contains("DONE"), "got:\n{out}");

    // The dynamic import must be a real split: an async chunk file exists
    // and the manifest maps the module key to it.
    let js_files: Vec<String> = std::fs::read_dir(t.path().join(".pledge"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".js"))
        .collect();
    assert!(
        js_files.iter().any(|n| n.contains("async")),
        "expected an async chunk, got: {js_files:?}"
    );
}

#[test]
fn emitted_bundle_executes_cjs_dependency_chain() {
    if !node_available() {
        eprintln!("node not on PATH — skipping");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(
        t.path(),
        "src/index.ts",
        r#"import answer from "fake-cjs";
const app = document.getElementById("root");
if (app) app.textContent = `answer=${answer}`;
"#,
    );
    // Fake CJS package: index.js requires a nested CJS file and exports a
    // value gated on process.env.NODE_ENV — exercises require discovery,
    // specifier rewriting, NODE_ENV define and default interop.
    write(
        t.path(),
        "node_modules/fake-cjs/package.json",
        r#"{ "name": "fake-cjs", "version": "1.0.0", "main": "index.js" }"#,
    );
    write(
        t.path(),
        "node_modules/fake-cjs/index.js",
        r#"var inner = require("./lib/inner.js");
if (process.env.NODE_ENV === "production") {
    module.exports = inner.answer;
} else {
    module.exports = -1;
}
"#,
    );
    write(
        t.path(),
        "node_modules/fake-cjs/lib/inner.js",
        "module.exports = { answer: 42 };\n",
    );

    let out = build_and_run(t.path(), "entry-");
    assert!(out.contains("DOM: answer=42"), "got:\n{out}");
    assert!(out.contains("DONE"), "got:\n{out}");
}

#[test]
fn emitted_bundle_executes_esm_dependency() {
    if !node_available() {
        eprintln!("node not on PATH — skipping");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(
        t.path(),
        "src/index.ts",
        r#"import { add } from "fake-esm";
const app = document.getElementById("root");
if (app) app.textContent = `sum=${add(2, 3)}`;
"#,
    );
    write(
        t.path(),
        "node_modules/fake-esm/package.json",
        r#"{ "name": "fake-esm", "version": "1.0.0", "type": "module", "main": "index.js" }"#,
    );
    write(
        t.path(),
        "node_modules/fake-esm/index.js",
        "export function add(a, b) { return a + b; }\n",
    );

    let out = build_and_run(t.path(), "entry-");
    assert!(out.contains("DOM: sum=5"), "got:\n{out}");
}

#[test]
fn emitted_bundle_executes_circular_dependencies() {
    if !node_available() {
        eprintln!("node not on PATH — skipping");
        return;
    }
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(
        t.path(),
        "src/index.ts",
        r#"import { a } from "./a";
const app = document.getElementById("root");
if (app) app.textContent = `a=${a()}`;
"#,
    );
    write(
        t.path(),
        "src/a.ts",
        r#"import { b } from "./b";
export function a(): string { return "a" + b(); }
"#,
    );
    write(
        t.path(),
        "src/b.ts",
        r#"import { a } from "./a";
export function b(): string { return "b"; }
export function callA(): string { return a(); }
"#,
    );

    let out = build_and_run(t.path(), "entry-");
    assert!(out.contains("DOM: a=ab"), "got:\n{out}");
}

#[test]
fn emitted_worker_bundle_is_written_at_its_url_path() {
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(
        t.path(),
        "src/index.ts",
        r#"const w = new Worker(new URL("./job.worker.ts", import.meta.url));
console.log("worker url constructed:", typeof w.postMessage === "function");
"#,
    );
    write(
        t.path(),
        "src/job.worker.ts",
        r#"import { helper } from "./helper";
self.onmessage = () => { postMessage(helper()); };
"#,
    );
    write(
        t.path(),
        "src/helper.ts",
        "export function helper(): string { return \"from-helper\"; }\n",
    );

    let out = pledge(t.path(), &["build"]);
    assert!(out.status.success(), "build failed:\n{}", text(&out));

    // The transform rewrites the URL to /src/job.worker.js — that file must
    // exist and contain the worker code plus its dependency closure.
    let worker = t.path().join(".pledge/src/job.worker.js");
    assert!(
        worker.exists(),
        "worker bundle missing at {}",
        worker.display()
    );
    let code = std::fs::read_to_string(&worker).unwrap();
    assert!(code.contains("postMessage"), "worker code missing:\n{code}");
    assert!(
        code.contains("from-helper") || code.contains("helper"),
        "worker dep closure missing:\n{code}"
    );
}

#[test]
fn build_verify_flag_passes_on_emitted_output() {
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(
        t.path(),
        "src/index.ts",
        "import { x } from './util';\nconsole.log(x);\nimport('./lazy');\n",
    );
    write(t.path(), "src/util.ts", "export const x = 1;\n");
    write(t.path(), "src/lazy.ts", "export const y = 2;\n");

    let out = pledge(t.path(), &["build", "--verify"]);
    assert!(
        out.status.success(),
        "pledge build --verify failed on valid output:\n{}",
        text(&out)
    );
}

#[test]
fn sri_integrity_attributes_are_injected_into_index_html() {
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    // Enable SRI via the security config block.
    write(
        t.path(),
        "pledge.config.ts",
        r#"export default {
  entry: ['src/index.ts'],
  outDir: '.pledge',
  security: { sri: true },
};
"#,
    );
    write(t.path(), "src/index.ts", "console.log('sri test');\n");

    let out = pledge(t.path(), &["build"]);
    assert!(out.status.success(), "build failed:\n{}", text(&out));

    let html = std::fs::read_to_string(t.path().join(".pledge/index.html")).unwrap();
    assert!(
        html.contains("integrity=\"sha256-"),
        "no SRI integrity attribute in index.html:\n{html}"
    );
}

#[test]
fn secret_scan_fails_build_on_hardcoded_token() {
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    // A GitHub-shaped token literal in source must fail the build —
    // recognized credential shapes are hard findings.
    write(
        t.path(),
        "src/index.ts",
        "const tok = \"ghp_abcdefghijklmnopqrstuvwxyz0123456789AB\";\nconsole.log(tok);\n",
    );

    let out = pledge(t.path(), &["build"]);
    assert!(
        !out.status.success(),
        "build should refuse to emit a GitHub token:\n{}",
        text(&out)
    );
    assert!(
        text(&out).contains("secret") || text(&out).contains("Secret"),
        "expected secret-scan error in output:\n{}",
        text(&out)
    );
}

#[test]
fn secret_scan_passes_clean_build() {
    let t = tempfile::tempdir().unwrap();
    scaffold(t.path());
    write(t.path(), "src/index.ts", "console.log('clean');\n");

    let out = pledge(t.path(), &["build"]);
    assert!(out.status.success(), "clean build failed:\n{}", text(&out));
}

#[test]
fn env_prefix_wildcard_is_rejected_at_build_time() {
    let t = tempfile::tempdir().unwrap();
    write(
        t.path(),
        "pledge.config.ts",
        r#"export default {
  entry: ['src/index.ts'],
  outDir: '.pledge',
  envPrefix: ['*'],
};
"#,
    );
    write(
        t.path(),
        "index.html",
        r#"<!DOCTYPE html><html><body><div id="root"></div>
<script src="/src/index.ts"></script></body></html>"#,
    );
    write(t.path(), "src/index.ts", "console.log('x');\n");

    let out = pledge(t.path(), &["build"]);
    assert!(
        !out.status.success(),
        "envPrefix ['*'] should fail the build:\n{}",
        text(&out)
    );
    assert!(
        text(&out).contains("envPrefix"),
        "expected envPrefix error:\n{}",
        text(&out)
    );
}
