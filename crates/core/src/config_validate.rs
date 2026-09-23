// Config validation — checks for unknown fields, typos, and provides "Did you mean...?" suggestions.

/// Valid framework values.
pub const VALID_FRAMEWORKS: &[&str] = &[
    "react", "vue", "svelte", "solid", "next", "tanstack", "astro", "pledge", "auto",
];

/// Valid output format values.
pub const VALID_OUTPUT_FORMATS: &[&str] = &["esm", "cjs", "iife"];

/// Valid edge target values.
pub const VALID_EDGE_TARGETS: &[&str] = &["cloudflare", "vercel", "deno"];

/// Valid preload strategy values.
pub const VALID_PRELOAD_STRATEGIES: &[&str] = &["eager", "lazy", "manual"];

/// Valid WASM SIMD values.
pub const VALID_WASM_SIMD_MODES: &[&str] = &["auto", "always", "never"];

/// Valid RTL mode values.
pub const VALID_RTL_MODES: &[&str] = &["auto", "manual", "off"];

/// Top-level keys owned by PledgeStack (the `pledge` runtime), which shares
/// `pledge.config.ts` with PledgePack when `framework: 'pledge'` — PledgePack
/// parses the same file as the engine underneath and must accept these
/// without warnings.
pub const PLEDGESTACK_FIELDS: &[&str] = &[
    "rootDir",
    "publicDir",
    "defaultRuntime",
    "rsc",
    "tailwind",
    "output",
    "middlewarePath",
    "pledgepack",
    "mdx",
    "cargo",
    "bundler",
    "siteUrl",
    "securityHeaders",
    "csrf",
    "ppr",
    "botDetection",
    "rateLimit",
    "bruteForceProtection",
    "trustedProxies",
    "supplyChain",
    "cdn",
    "geoRestriction",
    "cors",
    "csp",
    "cspReportOnly",
    "allowedRedirects",
    "authPaths",
    "maskForbidden",
    "compression",
    "envSchema",
];

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
    pub suggestion: Option<String>,
}

/// Validate config *values* (not just field names).
///
/// Checks ranges, cross-field requirements, and file existence for values
/// that cannot be caught by the field-name validation above.
pub fn validate_config_values(config: &serde_json::Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if let Some(obj) = config.as_object() {
        // Enum-value checks (the JSON schema types these as plain strings).
        for (key, valid) in [
            ("framework", VALID_FRAMEWORKS),
            ("outputFormat", VALID_OUTPUT_FORMATS),
            ("edgeTarget", VALID_EDGE_TARGETS),
        ] {
            if let Some(v) = obj.get(key).and_then(|v| v.as_str())
                && !valid.contains(&v)
            {
                let suggestion = find_closest_match(v, valid);
                errors.push(ValidationError {
                    field: key.to_string(),
                    message: format!("Invalid {}: '{}'", key, v),
                    suggestion: suggestion
                        .map(|s| format!("Did you mean '{}'? Valid: {}", s, valid.join(", "))),
                });
            }
        }
        if let Some(b_obj) = obj.get("build").and_then(|v| v.as_object()) {
            for (key, valid) in [
                ("preloadStrategy", VALID_PRELOAD_STRATEGIES),
                ("wasmSimd", VALID_WASM_SIMD_MODES),
            ] {
                if let Some(v) = b_obj.get(key).and_then(|v| v.as_str())
                    && !valid.contains(&v)
                {
                    let suggestion = find_closest_match(v, valid);
                    errors.push(ValidationError {
                        field: format!("build.{}", key),
                        message: format!("Invalid {}: '{}'", key, v),
                        suggestion: suggestion
                            .map(|s| format!("Did you mean '{}'? Valid: {}", s, valid.join(", "))),
                    });
                }
            }
        }
        if let Some(c_obj) = obj.get("css").and_then(|v| v.as_object())
            && let Some(rtl) = c_obj.get("rtl").and_then(|v| v.as_str())
            && !VALID_RTL_MODES.contains(&rtl)
        {
            let suggestion = find_closest_match(rtl, VALID_RTL_MODES);
            errors.push(ValidationError {
                field: "css.rtl".to_string(),
                message: format!("Invalid rtl mode: '{}'", rtl),
                suggestion: suggestion.map(|s| {
                    format!(
                        "Did you mean '{}'? Valid: {}",
                        s,
                        VALID_RTL_MODES.join(", ")
                    )
                }),
            });
        }

        // Validate devServer.port range (0-65535)
        if let Some(ds) = obj.get("devServer").and_then(|v| v.as_object())
            && let Some(port) = ds.get("port").and_then(|v| v.as_u64())
            && port > 65535
        {
            errors.push(ValidationError {
                field: "devServer.port".to_string(),
                message: format!("port must be 0-65535, got {}", port),
                suggestion: None,
            });
        }

        // Validate https config: if present, cert and key are required
        if let Some(https) = obj.get("https").and_then(|v| v.as_object()) {
            if https.get("cert").is_none() {
                errors.push(ValidationError {
                    field: "https.cert".to_string(),
                    message: "https config requires a cert path".to_string(),
                    suggestion: None,
                });
            }
            if https.get("key").is_none() {
                errors.push(ValidationError {
                    field: "https.key".to_string(),
                    message: "https config requires a key path".to_string(),
                    suggestion: None,
                });
            }
        }

        // Validate image.quality range (0-100)
        if let Some(image) = obj.get("image").and_then(|v| v.as_object())
            && let Some(quality) = image.get("quality").and_then(|v| v.as_u64())
            && quality > 100
        {
            errors.push(ValidationError {
                field: "image.quality".to_string(),
                message: format!("quality must be 0-100, got {}", quality),
                suggestion: None,
            });
        }

        // Validate htmlEntry file exists
        if let Some(entry) = obj.get("htmlEntry").and_then(|v| v.as_str())
            && !std::path::Path::new(entry).exists()
        {
            errors.push(ValidationError {
                field: "htmlEntry".to_string(),
                message: format!("htmlEntry file does not exist: {}", entry),
                suggestion: None,
            });
        }
    }

    errors
}

// --- Schema-driven unknown-field detection ---------------------------

/// Keys that are always accepted at the top level (editor / tooling metadata
/// and legacy serde aliases).
const ALWAYS_ALLOWED: &[&str] = &["$schema", "output_dir"];

/// Report keys in a raw config object that the config schema (the same one
/// `pledge schema` prints) does not define, at any nesting depth, with a
/// did-you-mean suggestion (Levenshtein distance) for near misses.
pub fn find_unknown_fields(raw: &serde_json::Value) -> Vec<ValidationError> {
    let Ok(schema) = crate::generate_config_schema() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk_schema(raw, &schema, &schema, "", &mut out, 0);
    // `framework: 'pledge'` configs are shared with the PledgeStack runtime —
    // its framework-level keys are legal even though the bundler schema
    // doesn't define them.
    let is_pledgestack = raw
        .get("framework")
        .and_then(|f| f.as_str())
        .map(|f| f == "pledge")
        .unwrap_or(false);
    if is_pledgestack {
        out.retain(|e| {
            !(e.field.split('.').count() == 1 && PLEDGESTACK_FIELDS.contains(&e.field.as_str()))
        });
    }
    out
}

fn resolve_ref<'a>(
    schema: &'a serde_json::Value,
    root: &'a serde_json::Value,
) -> &'a serde_json::Value {
    let mut cur = schema;
    for _ in 0..8 {
        match cur.get("$ref").and_then(|r| r.as_str()) {
            Some(r) => match root.pointer(r.trim_start_matches('#')) {
                Some(next) => cur = next,
                None => break,
            },
            None => break,
        }
    }
    cur
}

fn walk_schema(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
    out: &mut Vec<ValidationError>,
    depth: usize,
) {
    if depth > 12 {
        return;
    }
    let schema = resolve_ref(schema, root);

    // Unions (Option<T>, untagged enums): keep the applicable alternative that
    // reports the fewest problems for this value.
    for key in ["anyOf", "oneOf"] {
        if let Some(alts) = schema.get(key).and_then(|a| a.as_array()) {
            let mut best: Option<Vec<ValidationError>> = None;
            for alt in alts {
                let alt_resolved = resolve_ref(alt, root);
                if value.is_object() && alt_resolved.get("properties").is_none() {
                    continue;
                }
                let mut trial = Vec::new();
                walk_schema(value, alt, root, path, &mut trial, depth + 1);
                if best.as_ref().is_none_or(|b| trial.len() < b.len()) {
                    best = Some(trial);
                }
            }
            out.extend(best.unwrap_or_default());
            return;
        }
    }
    if let Some(all) = schema.get("allOf").and_then(|a| a.as_array()) {
        for part in all {
            walk_schema(value, part, root, path, out, depth + 1);
        }
    }

    match value {
        serde_json::Value::Object(map) => {
            // Un-evaluated placeholders from static config evaluation.
            if map.contains_key(crate::js_config::CALL_KEY)
                || map.contains_key(crate::js_config::EXPR_KEY)
            {
                return;
            }
            let props = schema.get("properties").and_then(|p| p.as_object());
            let additional = schema.get("additionalProperties");
            for (key, val) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                match props.and_then(|p| p.get(key)) {
                    Some(child_schema) => {
                        walk_schema(val, child_schema, root, &child_path, out, depth + 1)
                    }
                    None => {
                        if let Some(props) = props {
                            let allowed = path.is_empty() && ALWAYS_ALLOWED.contains(&key.as_str());
                            let open = matches!(
                                additional,
                                Some(a) if a != &serde_json::Value::Bool(false)
                            );
                            if !allowed && !open {
                                let names: Vec<&str> = props.keys().map(|k| k.as_str()).collect();
                                let suggestion = find_closest_match(key, &names);
                                let kind = if path.is_empty() {
                                    "config field".to_string()
                                } else {
                                    format!("{} field", path)
                                };
                                out.push(ValidationError {
                                    field: child_path,
                                    message: format!("Unknown {}: '{}'", kind, key),
                                    suggestion: suggestion
                                        .map(|s| format!("Did you mean '{}'?", s)),
                                });
                            }
                        } else if let Some(add) = additional
                            && add.is_object()
                        {
                            walk_schema(val, add, root, &child_path, out, depth + 1);
                        }
                    }
                }
            }
        }
        serde_json::Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    walk_schema(
                        item,
                        item_schema,
                        root,
                        &format!("{}[{}]", path, i),
                        out,
                        depth + 1,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Guidance for a config that names no entry point and has no HTML entry to
/// discover one from. `None` when the entry configuration is usable.
pub fn missing_entry_guidance(config: &crate::config::PledgeConfig) -> Option<String> {
    if !config.entry.is_empty()
        || config.app_dir.is_some()
        || config
            .resolve_app_dir()
            .is_some_and(|d| config.root.join(d).is_dir())
    {
        return None;
    }
    let html = config
        .html_entry
        .clone()
        .unwrap_or_else(|| "index.html".to_string());
    if config.root.join(&html).is_file() {
        return None;
    }
    let has_convention = config.resolve_base_dir().is_some_and(|dir| {
        let base = config.root.join(dir);
        ["index", "main", "entry"].iter().any(|n| {
            ["tsx", "ts", "jsx", "js"]
                .iter()
                .any(|e| base.join(format!("{}.{}", n, e)).is_file())
        })
    });
    if has_convention {
        return None;
    }
    Some(format!(
        "No entry point: `entry` is empty and no {html} was found in {root}.\n  \
         Fix it by one of:\n    \
         - set `entry: ['src/index.tsx']` in pledge.config.ts (or \"entry\": [...] in pledge.json)\n    \
         - add an {html} that loads your entry with <script type=\"module\" src=\"/src/index.tsx\">\n    \
         - run `pledgepack init` to detect and generate a config",
        root = crate::display_path(&config.root),
        html = html
    ))
}

/// Find the closest matching string using Levenshtein distance.
pub fn find_closest_match(input: &str, candidates: &[&str]) -> Option<String> {
    let input_lower = input.to_lowercase();
    let mut best: Option<(usize, &str)> = None;

    for candidate in candidates {
        let dist = levenshtein(&input_lower, &candidate.to_lowercase());
        if best.is_none_or(|(d, _)| dist < d) {
            best = Some((dist, candidate));
        }
    }

    // Only suggest if distance is reasonable (<= half the input length)
    if let Some((dist, candidate)) = best {
        let max_dist = (input.len() / 2).max(2);
        if dist <= max_dist {
            return Some(candidate.to_string());
        }
    }

    None
}

/// Levenshtein distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr: Vec<usize> = vec![0; b_len + 1];

    for i in 1..=a_len {
        curr[0] = i;
        for j in 1..=b_len {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

/// Format validation errors for CLI output.
pub fn format_errors(errors: &[ValidationError]) -> String {
    let mut output = String::new();
    for err in errors {
        output.push_str(&format!(
            "  \x1b[33m⚠\x1b[0m {}\n     {}",
            err.field, err.message
        ));
        if let Some(ref suggestion) = err.suggestion {
            output.push_str(&format!("\n     \x1b[36m{}\x1b[0m", suggestion));
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_defined_fields_pass() {
        // Every key here exists in the generated config schema — the schema is
        // the single source of truth, so adding a field to PledgeConfig makes
        // it valid with no changes to this file.
        let raw = json!({
            "entry": ["src/index.tsx"],
            "outDir": "dist",
            "framework": "react",
            "devServer": { "port": 3000, "host": "localhost", "hmr": true },
            "build": { "preloadStrategy": "lazy" },
            "optimize": { "minify": true },
            "cache": { "enabled": false },
            "resolve": { "alias": { "@": "./src" } }
        });
        assert!(
            find_unknown_fields(&raw).is_empty(),
            "{:?}",
            find_unknown_fields(&raw)
        );
    }

    #[test]
    fn unknown_top_level_field_is_flagged_with_suggestion() {
        let raw = json!({ "outdir": "dist" });
        let errors = find_unknown_fields(&raw);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "outdir");
        assert!(errors[0].message.contains("Unknown"));
        assert!(
            errors[0]
                .suggestion
                .as_deref()
                .is_some_and(|s| s.contains("outDir")),
            "{:?}",
            errors[0].suggestion
        );
    }

    #[test]
    fn unknown_nested_field_is_flagged() {
        let raw = json!({ "devServer": { "prt": 3000 } });
        let errors = find_unknown_fields(&raw);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "devServer.prt");
        assert!(
            errors[0]
                .suggestion
                .as_deref()
                .is_some_and(|s| s.contains("port")),
            "{:?}",
            errors[0].suggestion
        );
    }

    #[test]
    fn always_allowed_keys_pass() {
        // $schema (editor metadata) and output_dir (legacy serde alias) are
        // intentionally outside the generated schema but must not warn.
        let raw = json!({ "$schema": "./.pp-schema.json", "output_dir": "dist" });
        assert!(find_unknown_fields(&raw).is_empty());
    }

    #[test]
    fn pledgestack_fields_pass_only_for_pledge_framework() {
        let key = PLEDGESTACK_FIELDS[0];
        let stack = json!({ "framework": "pledge", key: true });
        assert!(
            find_unknown_fields(&stack).is_empty(),
            "PledgeStack key `{key}` must pass with framework: 'pledge'"
        );
        // Same key without the pledge framework is an ordinary unknown field.
        let other = json!({ "framework": "react", key: true });
        assert_eq!(
            find_unknown_fields(&other).len(),
            1,
            "PledgeStack key `{key}` must warn without framework: 'pledge'"
        );
    }

    #[test]
    fn validation_tracks_the_generated_schema() {
        // The schema itself drives validation: take a real property name out
        // of the generated schema and assert it is accepted — no allowlist in
        // this file is consulted.
        let schema = crate::generate_config_schema().unwrap();
        let props = schema["properties"].as_object().unwrap();
        let field = props.keys().next().unwrap().clone();
        let raw = json!({ field.clone(): serde_json::Value::Null });
        assert!(
            find_unknown_fields(&raw)
                .iter()
                .all(|e| !e.message.contains("Unknown")),
            "schema property `{field}` must be accepted"
        );
    }

    #[test]
    fn value_checks_still_catch_bad_enums_and_ranges() {
        let raw = json!({
            "framework": "raect",
            "devServer": { "port": 70000 },
            "build": { "preloadStrategy": "sometimes" }
        });
        let errors = validate_config_values(&raw);
        let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"framework"), "{fields:?}");
        assert!(fields.contains(&"devServer.port"), "{fields:?}");
        assert!(fields.contains(&"build.preloadStrategy"), "{fields:?}");
        assert!(
            errors
                .iter()
                .any(|e| e.suggestion.as_deref().is_some_and(|s| s.contains("react"))),
            "framework typo should suggest 'react'"
        );
    }
}
