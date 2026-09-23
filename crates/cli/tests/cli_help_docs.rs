//! CLI ↔ documentation drift gate.
//!
//! Two halves:
//!
//! 1. Help snapshots — `--help` output for the binary and every subcommand is
//!    compared against `tests/snapshots/help/*.txt`. Regenerate with:
//!    `PLEDGE_UPDATE_SNAPSHOTS=1 cargo test -p pledgepack-cli --test cli_help_docs`
//!
//! 2. Docs command check — every `` `pledgepack <args>` `` invocation written in
//!    README.md / docs/*.md must reference a command path that actually exists
//!    (derived live from `pledge <path> --help`, so adding or renaming a
//!    `Commands` variant automatically updates the valid set — no second
//!    hand-maintained list to drift).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const UPDATE_ENV: &str = "PLEDGE_UPDATE_SNAPSHOTS";

fn pledge_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pledge")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn snapshot_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/help")
}

fn run_help(path: &[String]) -> String {
    let mut cmd = Command::new(pledge_bin());
    cmd.args(path).arg("--help");
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to run `pledge {} --help`: {e}", path.join(" ")));
    assert!(
        out.status.success(),
        "`pledge {} --help` exited with {}",
        path.join(" "),
        out.status
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

/// Parse the `Commands:` section of clap help text into subcommand names.
fn subcommands_of(help: &str) -> Vec<String> {
    let mut in_commands = false;
    let mut names = Vec::new();
    for line in help.lines() {
        if in_commands {
            let trimmed = line.trim();
            if trimmed.is_empty() || !line.starts_with(' ') {
                break;
            }
            if let Some(name) = trimmed.split_whitespace().next()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                names.push(name.to_string());
            }
        } else if line.trim_end() == "Commands:" {
            in_commands = true;
        }
    }
    names
}

/// Discover the full command tree by running `--help` recursively.
fn command_tree() -> BTreeMap<Vec<String>, String> {
    let mut tree: BTreeMap<Vec<String>, String> = BTreeMap::new();
    let mut queue: Vec<Vec<String>> = vec![Vec::new()];
    while let Some(path) = queue.pop() {
        if tree.contains_key(&path) {
            continue;
        }
        let help = run_help(&path);
        for sub in subcommands_of(&help) {
            // `help` is clap's generated pseudo-subcommand — `pledge help
            // --help` exits 2, so it cannot be discovered recursively. It is
            // still accepted in docs via `valid_command_paths`.
            if sub == "help" {
                continue;
            }
            let mut child = path.clone();
            child.push(sub);
            queue.push(child);
        }
        tree.insert(path, help);
    }
    tree
}

fn snapshot_name(path: &[String]) -> String {
    if path.is_empty() {
        "root.txt".to_string()
    } else {
        format!("{}.txt", path.join("__"))
    }
}

#[test]
fn help_output_matches_snapshots() {
    let update = std::env::var(UPDATE_ENV).is_ok();
    let dir = snapshot_dir();
    if update {
        std::fs::create_dir_all(&dir).unwrap();
    }
    let tree = command_tree();
    let mut failures = Vec::new();
    for (path, help) in &tree {
        let file = dir.join(snapshot_name(path));
        if update {
            std::fs::write(&file, help).unwrap();
            continue;
        }
        if !file.is_file() {
            failures.push(format!(
                "missing snapshot for `pledge {}` — run with {UPDATE_ENV}=1 to create {}",
                path.join(" "),
                file.display()
            ));
            continue;
        }
        let expected = std::fs::read_to_string(&file)
            .unwrap()
            .replace("\r\n", "\n");
        if expected != *help {
            failures.push(format!(
                "`pledge {} --help` changed — if intentional, regenerate snapshots \
                 with {UPDATE_ENV}=1\n--- expected ---\n{}\n--- actual ---\n{}",
                path.join(" "),
                expected,
                help
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Command paths the docs may reference: every path in the discovered tree
/// except the implicit root.
fn valid_command_paths(tree: &BTreeMap<Vec<String>, String>) -> Vec<Vec<String>> {
    let mut paths: Vec<Vec<String>> = tree.keys().filter(|p| !p.is_empty()).cloned().collect();
    // clap's generated `help` pseudo-subcommand is valid but undiscoverable.
    paths.push(vec!["help".to_string()]);
    paths
}

/// Markdown files the check covers: README.md + docs/*.md (non-recursive).
fn doc_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut files = vec![root.join("README.md")];
    let docs = root.join("docs");
    if docs.is_dir() {
        for entry in std::fs::read_dir(&docs).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|e| e.to_str()) == Some("md") {
                files.push(p);
            }
        }
    }
    files
}

/// Extract `pledgepack ...` invocations from markdown: backtick spans whose
/// command token is `pledgepack`, and fenced-code lines that start with
/// `pledgepack` (optionally after `$`, `npx`, `pnpm`, `bunx`, `yarn`, `dlx`).
/// Returns (file-line, args-text).
fn pledgepack_invocations(text: &str) -> Vec<(usize, String)> {
    const RUNNERS: &[&str] = &["$", "npx", "pnpx", "pnpm", "bunx", "yarn", "dlx", "npm"];
    let mut out = Vec::new();
    let mut in_fence = false;
    for (idx, line) in text.lines().enumerate() {
        let lineno = idx + 1;
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            let tokens: Vec<&str> = line.trim().split_whitespace().collect();
            if let Some(pos) = tokens.iter().position(|t| *t == "pledgepack")
                && tokens[..pos].iter().all(|t| RUNNERS.contains(t))
            {
                out.push((lineno, tokens[pos + 1..].join(" ")));
            }
            continue;
        }
        // Inline code spans.
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            let span = &after[..end];
            let tokens: Vec<&str> = span.split_whitespace().collect();
            if let Some(pos) = tokens.iter().position(|t| *t == "pledgepack")
                && tokens[..pos].iter().all(|t| RUNNERS.contains(t))
            {
                out.push((lineno, tokens[pos + 1..].join(" ")));
            }
            rest = &after[end + 1..];
        }
    }
    out
}

/// Validate one `pledgepack <args>` invocation against the command tree.
/// Returns Err(reason) when the doc references something that cannot exist.
fn check_invocation(
    args: &str,
    paths: &[Vec<String>],
    tree: &BTreeMap<Vec<String>, String>,
) -> Result<(), String> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    let mut matched: Vec<String> = Vec::new();
    for token in &tokens {
        if token.starts_with('-') {
            break; // flags end the command path
        }
        let mut candidate = matched.clone();
        candidate.push((*token).to_string());
        if paths.contains(&candidate) {
            matched = candidate;
            continue;
        }
        // Not a valid continuation — is it a positional arg or a mistake?
        let node = tree.get(&matched).cloned().unwrap_or_default();
        let has_subcommands = !subcommands_of(&node).is_empty();
        if has_subcommands {
            return Err(format!(
                "`{token}` is not a subcommand of `pledgepack {}`",
                matched.join(" ")
            ));
        }
        break; // leaf command — remaining tokens are positional args
    }
    Ok(())
}

#[test]
fn docs_only_reference_real_commands() {
    let tree = command_tree();
    let paths = valid_command_paths(&tree);
    let mut failures = Vec::new();
    for file in doc_files() {
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (lineno, args) in pledgepack_invocations(&text) {
            if args.is_empty() {
                continue; // bare `pledgepack`
            }
            if let Err(reason) = check_invocation(&args, &paths, &tree) {
                failures.push(format!(
                    "{}:{}: `pledgepack {}` — {}",
                    file.display(),
                    lineno,
                    args,
                    reason
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "documentation references commands that do not exist:\n{}",
        failures.join("\n")
    );
}
