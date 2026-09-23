//! `package.json` `exports` / `imports` map matching, shared by the engine and
//! the standalone resolver.
//!
//! This module is pure (no file-system access): it only turns a package map
//! plus a request key into a *target string*; turning that into a file is the
//! caller's job. It lives in `pledgepack-resolver` — the single resolution
//! implementation — and `pledgepack-core` re-exports it as
//! `pledgepack_core::package_map` for API compatibility.
//!
//! Semantics follow Node's package entry points:
//!
//! * string / array / conditions-object targets, nested to any depth;
//! * `*` subpath patterns, best match by longest prefix then longest key
//!   (Node's `PATTERN_KEY_COMPARE`), never letting the matched text smuggle in
//!   `..` segments;
//! * targets must stay inside the package (`./…`, no `..`, no nested
//!   `node_modules`); `imports` targets may additionally be bare package
//!   specifiers (`"#dep": "dep-pkg"`).
//!
//! Condition selection honours the caller's priority order, then `default`
//! (serde_json's default map does not preserve key order, so the package's own
//! key order cannot be used).

use serde_json::{Map, Value};

/// Among `keys` containing exactly one `*`, find the best pattern match for
/// `target`: longest prefix before the `*`, then longest key. Returns the key
/// and the text the `*` matched.
pub fn best_pattern_match<'a>(
    keys: impl Iterator<Item = &'a str>,
    target: &str,
) -> Option<(&'a str, String)> {
    let mut best: Option<(&str, usize, String)> = None;
    for key in keys {
        if key.matches('*').count() != 1 {
            continue;
        }
        let (prefix, suffix) = key.split_once('*').expect("one star");
        if target.len() >= key.len() - 1
            && target.starts_with(prefix)
            && target.ends_with(suffix)
            && target != prefix
        {
            let star = &target[prefix.len()..target.len() - suffix.len()];
            // the matched text must not smuggle in `..` segments
            if star.split('/').any(|s| s == "..") {
                continue;
            }
            let better = match &best {
                None => true,
                Some((bk, bp, _)) => {
                    prefix.len() > *bp || (prefix.len() == *bp && key.len() > bk.len())
                }
            };
            if better {
                best = Some((key, prefix.len(), star.to_string()));
            }
        }
    }
    best.map(|(k, _, star)| (k, star))
}

/// A `./…` target that stays inside the package.
fn is_safe_relative(s: &str) -> bool {
    match s.strip_prefix("./") {
        Some(rel) => !rel
            .split(['/', '\\'])
            .any(|seg| seg == ".." || seg.eq_ignore_ascii_case("node_modules")),
        None => false,
    }
}

/// A bare package specifier usable as an `imports` target.
fn is_bare_specifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.starts_with('/')
        && !s.starts_with('#')
        && !s.contains('\\')
        && !s.split('/').any(|seg| seg == "..")
}

/// Resolve a target value (string, array of fallbacks, conditions object,
/// `null`) to a concrete target string.
///
/// * `star` substitutes `*` in pattern targets.
/// * `allow_bare` accepts bare package specifiers (only valid for `imports`).
pub fn resolve_target(
    value: &Value,
    star: Option<&str>,
    conditions: &[String],
    allow_bare: bool,
) -> Option<String> {
    match value {
        Value::String(s) => {
            let s = match star {
                Some(m) => s.replace('*', m),
                None => s.clone(),
            };
            if is_safe_relative(&s) || (allow_bare && is_bare_specifier(&s)) {
                Some(s)
            } else {
                None
            }
        }
        Value::Array(items) => items
            .iter()
            .find_map(|v| resolve_target(v, star, conditions, allow_bare)),
        Value::Object(obj) => {
            for cond in conditions {
                if let Some(v) = obj.get(cond)
                    && let Some(r) = resolve_target(v, star, conditions, allow_bare)
                {
                    return Some(r);
                }
            }
            obj.get("default")
                .and_then(|v| resolve_target(v, star, conditions, allow_bare))
        }
        _ => None,
    }
}

/// Look `key` up in a subpath map (`exact key`, else best `*` pattern).
fn lookup_in_map(
    map: &Map<String, Value>,
    key: &str,
    conditions: &[String],
    allow_bare: bool,
) -> Option<String> {
    if !key.contains('*')
        && let Some(v) = map.get(key)
    {
        return resolve_target(v, None, conditions, allow_bare);
    }
    let (matched_key, star) = best_pattern_match(map.keys().map(String::as_str), key)?;
    resolve_target(&map[matched_key], Some(&star), conditions, allow_bare)
}

/// Resolve a package.json `exports` entry for `key` (`"."` or `"./sub"`).
///
/// Supports the sugar forms (string / array / bare condition object), subpath
/// maps, nested conditions and `*` subpath patterns.
pub fn resolve_exports_entry(exports: &Value, key: &str, conditions: &[String]) -> Option<String> {
    let is_subpath_map =
        matches!(exports, Value::Object(o) if o.keys().any(|k| k.starts_with('.')));
    if !is_subpath_map {
        return if key == "." {
            resolve_target(exports, None, conditions, false)
        } else {
            None
        };
    }
    lookup_in_map(exports.as_object()?, key, conditions, false)
}

/// Resolve a `#`-prefixed package `imports` specifier against a package.json
/// `imports` map. The result is either a package-relative `./…` path or a bare
/// package specifier (`"#dep": "dep-pkg"`).
pub fn resolve_imports_entry(
    imports: &Map<String, Value>,
    specifier: &str,
    conditions: &[String],
) -> Option<String> {
    // Only `#`-keyed entries are valid import specifiers, and the specifier
    // itself must be `#` + something ("#" and "#/" are invalid in Node).
    if !specifier.starts_with('#') || specifier == "#" || specifier.starts_with("#/") {
        return None;
    }
    lookup_in_map(imports, specifier, conditions, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn conds(c: &[&str]) -> Vec<String> {
        c.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exports_sugar_subpaths_conditions_and_patterns() {
        let c = conds(&["import"]);
        assert_eq!(
            resolve_exports_entry(&json!("./index.js"), ".", &c).as_deref(),
            Some("./index.js")
        );
        assert_eq!(resolve_exports_entry(&json!("./index.js"), "./x", &c), None);
        let map = json!({
            ".": {"types": "./t.d.ts", "require": "./cjs.js", "import": "./esm.js", "default": "./d.js"},
            "./feat/*": {"import": {"types": "./x.d.ts", "default": "./lib/*.mjs"}},
            "./evil": "../../secret.js",
            "./arr": ["../bad.js", "./ok.js"],
        });
        assert_eq!(
            resolve_exports_entry(&map, ".", &c).as_deref(),
            Some("./esm.js")
        );
        assert_eq!(
            resolve_exports_entry(&map, "./feat/a/b", &c).as_deref(),
            Some("./lib/a/b.mjs")
        );
        assert_eq!(resolve_exports_entry(&map, "./evil", &c), None);
        assert_eq!(
            resolve_exports_entry(&map, "./arr", &c).as_deref(),
            Some("./ok.js")
        );
        // A pattern star may not smuggle in `..`.
        assert_eq!(resolve_exports_entry(&map, "./feat/../../x", &c), None);
    }

    #[test]
    fn imports_exact_pattern_conditions_and_bare_targets() {
        let imports = json!({
            "#utils/*": "./src/utils/*.js",
            "#dep": "dep-pkg",
            "#scoped": "@scope/pkg/sub",
            "#cond": {"node": "./n.js", "browser": "./b.js", "default": "./d.js"},
            "#exact": "./exact.js",
            "#bad": "../escape.js",
            "#nm": "./node_modules/x/index.js",
            "#loop": "#exact",
        });
        let imports = imports.as_object().unwrap();
        let c = conds(&["browser"]);
        assert_eq!(
            resolve_imports_entry(imports, "#utils/a/b", &c).as_deref(),
            Some("./src/utils/a/b.js")
        );
        assert_eq!(
            resolve_imports_entry(imports, "#dep", &c).as_deref(),
            Some("dep-pkg")
        );
        assert_eq!(
            resolve_imports_entry(imports, "#scoped", &c).as_deref(),
            Some("@scope/pkg/sub")
        );
        assert_eq!(
            resolve_imports_entry(imports, "#cond", &c).as_deref(),
            Some("./b.js")
        );
        assert_eq!(
            resolve_imports_entry(imports, "#cond", &conds(&[])).as_deref(),
            Some("./d.js")
        );
        assert_eq!(
            resolve_imports_entry(imports, "#exact", &c).as_deref(),
            Some("./exact.js")
        );
        // Escapes, nested node_modules, `#`-to-`#` chains and unknown keys are refused.
        assert_eq!(resolve_imports_entry(imports, "#bad", &c), None);
        assert_eq!(resolve_imports_entry(imports, "#nm", &c), None);
        assert_eq!(resolve_imports_entry(imports, "#loop", &c), None);
        assert_eq!(resolve_imports_entry(imports, "#missing", &c), None);
        // Not a `#` specifier / invalid forms.
        assert_eq!(resolve_imports_entry(imports, "dep", &c), None);
        assert_eq!(resolve_imports_entry(imports, "#", &c), None);
        assert_eq!(resolve_imports_entry(imports, "#/x", &c), None);
    }

    #[test]
    fn best_pattern_prefers_longest_prefix() {
        let keys = ["./a/*", "./a/b/*", "./*"];
        let (k, star) = best_pattern_match(keys.iter().copied(), "./a/b/c").unwrap();
        assert_eq!((k, star.as_str()), ("./a/b/*", "c"));
    }
}
