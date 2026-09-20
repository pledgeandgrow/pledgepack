//! End-to-end: `pledge build` driving a real JS plugin's per-module hooks
//! (resolveId / load / transform / renderChunk / transformIndexHtml), run as
//! a subprocess with `--root` pointing at a project that is NOT the working
//! directory (which also covers the `--root` output-anchoring fix).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn project(plugin_src: &str, index_src: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "pledge.json",
        r#"{
            "entry": ["src/index.ts"],
            "outDir": "dist",
            "plugins": ["plugins/e2e-plugin.js"],
            "pluginSecurity": { "requireSigned": false },
            "cache": { "enabled": false },
            "sourceMaps": false
        }"#,
    );
    write(root, "plugins/e2e-plugin.js", plugin_src);
    write(root, "src/index.ts", index_src);
    tmp
}

/// Run `pledge --root <root> build` from a *different* working directory.
fn pledge_build(root: &Path) -> (Output, tempfile::TempDir) {
    let cwd = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_pledge"))
        .current_dir(cwd.path())
        .arg("--root")
        .arg(root)
        .arg("build")
        .output()
        .expect("failed to run pledge");
    (out, cwd)
}

fn find_js(dist: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dist)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "js"))
        .collect()
}

const PLUGIN: &str = r#"
export default {
  name: 'e2e-plugin',
  resolveId(source, importer) {
    if (source === 'virtual:version') return 'virtual:version';
    if (source === 'https-cdn') return { id: 'https-cdn', external: true };
    return null;
  },
  load(id) {
    if (id === 'virtual:version') return "export const version = '1.2.3';";
    return null;
  },
  transform(code, id) {
    if (id.endsWith('index.ts')) return code.replace('__BUILD__', '"plugin-transformed"');
    return null;
  },
  renderChunk(code, filename, type) {
    return '/* rendered-by-plugin ' + filename + ' ' + type + ' */\n' + code;
  },
  transformIndexHtml(html) {
    return [{ tag: 'meta', attrs: { name: 'plugin-meta', content: 'yes' }, injectTo: 'head' }];
  },
};
"#;

#[test]
fn js_plugin_hooks_shape_the_production_build() {
    let tmp = project(
        PLUGIN,
        "import { version } from 'virtual:version';\nimport 'https-cdn';\nexport const build = __BUILD__;\nconsole.log(version, build);\n",
    );
    let (out, _cwd) = pledge_build(tmp.path());
    assert!(
        out.status.success(),
        "build failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // `--root` was honoured: output landed under the project, not the cwd.
    let dist = tmp.path().join("dist");
    assert!(dist.join("index.html").exists(), "dist not under --root");

    let js = find_js(&dist);
    assert!(!js.is_empty(), "no chunk emitted");
    let all: String = js
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    // resolveId + load: the virtual module's code is in the bundle.
    assert!(all.contains("1.2.3"), "virtual module missing:\n{all}");
    // transform: source rewritten before compilation.
    assert!(
        all.contains("plugin-transformed"),
        "transform not applied:\n{all}"
    );
    // renderChunk: every chunk got the banner, with real filename + type.
    for p in &js {
        let c = std::fs::read_to_string(p).unwrap();
        assert!(
            c.starts_with("/* rendered-by-plugin "),
            "{}: {c}",
            p.display()
        );
    }
    assert!(
        all.contains(" entry */"),
        "entry chunk type not passed:\n{all}"
    );

    // transformIndexHtml: tag injected, and the HTML still references the
    // hashed chunk (hash computed after renderChunk).
    let html = std::fs::read_to_string(dist.join("index.html")).unwrap();
    assert!(
        html.contains(r#"name="plugin-meta""#) && html.contains(r#"content="yes""#),
        "{html}"
    );
    let entry_name = js
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .find(|n| html.contains(n.as_str()))
        .expect("html references no emitted chunk");
    assert!(entry_name.ends_with(".js"));
}

#[test]
fn plugin_error_aborts_the_build_with_plugin_name_and_file() {
    let tmp = project(
        "export default { name: 'explosive-plugin', transform(code, id) { throw new Error('kaboom in ' + id); } };",
        "export const x = 1;\n",
    );
    let (out, _cwd) = pledge_build(tmp.path());
    assert!(
        !out.status.success(),
        "build must fail when a plugin throws"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let all = format!("{stdout}\n{stderr}");
    assert!(all.contains("explosive-plugin"), "{all}");
    assert!(all.contains("kaboom"), "{all}");
    assert!(all.contains("index.ts"), "{all}");
}
