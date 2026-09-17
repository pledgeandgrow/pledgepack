// Config validation — checks for unknown fields, typos, and provides "Did you mean...?" suggestions.

/// All valid top-level config field names (camelCase as they appear in pledge.config.ts).
pub const VALID_FIELDS: &[&str] = &[
    "entry",
    "outDir",
    "root",
    "mode",
    "framework",
    "alias",
    "extensions",
    "cache",
    "devServer",
    "sourceMaps",
    "resolveAlias",
    "proxy",
    "profile",
    "outputFormat",
    "conditions",
    "envPrefix",
    "envDts",
    "htmlEntry",
    "compressGzip",
    "compressBrotli",
    "image",
    "edgeTarget",
    "plugins",
    "appDir",
    "build",
    "webhooks",
    "i18n",
    "css",
    "a11y",
    "encrypt",
    "budgets",
];

/// Valid devServer fields.
pub const VALID_DEV_SERVER_FIELDS: &[&str] = &["port", "host", "hmr", "open", "https"];

/// Valid cache fields.
pub const VALID_CACHE_FIELDS: &[&str] = &["enabled", "dir"];

/// Valid image fields.
pub const VALID_IMAGE_FIELDS: &[&str] = &[
    "enabled",
    "quality",
    "webp",
    "avif",
    "maxWidth",
    "maxHeight",
];

/// Valid framework values.
pub const VALID_FRAMEWORKS: &[&str] = &[
    "react", "vue", "svelte", "solid", "next", "tanstack", "astro", "pledge", "auto",
];

/// Valid output format values.
pub const VALID_OUTPUT_FORMATS: &[&str] = &["esm", "cjs", "iife"];

/// Valid edge target values.
pub const VALID_EDGE_TARGETS: &[&str] = &["cloudflare", "vercel", "deno"];

/// Valid build fields.
pub const VALID_BUILD_FIELDS: &[&str] = &[
    "manualChunks",
    "inlineDynamicImports",
    "sourceMapMode",
    "assetsInlineLimit",
    "jsonMinify",
    "modulePreload",
    "preload",
    "prefetch",
    "modulePreloadPolyfill",
    "fontSubsetting",
    "svgSprite",
    "envInline",
    "preloadStrategy",
    "verifyOutput",
    "incrementalOutput",
    "wasmSimd",
];

/// Valid preload strategy values.
pub const VALID_PRELOAD_STRATEGIES: &[&str] = &["eager", "lazy", "manual"];

/// Valid WASM SIMD values.
pub const VALID_WASM_SIMD_MODES: &[&str] = &["auto", "always", "never"];

/// Valid webhook fields.
pub const VALID_WEBHOOK_FIELDS: &[&str] = &["enabled", "onBuild", "onError", "headers"];

/// Valid i18n fields.
pub const VALID_I18N_FIELDS: &[&str] = &["enabled", "locales", "defaultLocale", "messagePattern"];

/// Valid CSS fields.
pub const VALID_CSS_FIELDS: &[&str] = &["rtl"];

/// Valid RTL mode values.
pub const VALID_RTL_MODES: &[&str] = &["auto", "manual", "off"];

/// Valid a11y fields.
pub const VALID_A11Y_FIELDS: &[&str] = &[
    "enabled",
    "failOnError",
    "checkAlt",
    "checkAria",
    "checkContrast",
];

/// Valid encrypt fields.
pub const VALID_ENCRYPT_FIELDS: &[&str] = &["enabled", "keys", "key"];

/// Valid budget fields.
pub const VALID_BUDGET_FIELDS: &[&str] = &[
    "enabled",
    "maxBundleSize",
    "maxChunkSize",
    "maxChunkCount",
    "entryBudgets",
];

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
    pub suggestion: Option<String>,
}

/// Validate a raw JSON config object (parsed from pledge.config.ts or pledge.json).
/// Returns a list of errors and warnings.
pub fn validate_config_json(config: &serde_json::Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if let Some(obj) = config.as_object() {
        for (key, _value) in obj {
            // Check top-level fields
            if !VALID_FIELDS.contains(&key.as_str()) {
                let suggestion = find_closest_match(key, VALID_FIELDS);
                errors.push(ValidationError {
                    field: key.clone(),
                    message: format!("Unknown config field: '{}'", key),
                    suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                });
            }

            // Validate nested devServer fields
            if key == "devServer"
                && let Some(ds_obj) = _value.as_object()
            {
                for ds_key in ds_obj.keys() {
                    if !VALID_DEV_SERVER_FIELDS.contains(&ds_key.as_str()) {
                        let suggestion = find_closest_match(ds_key, VALID_DEV_SERVER_FIELDS);
                        errors.push(ValidationError {
                            field: format!("devServer.{}", ds_key),
                            message: format!("Unknown devServer field: '{}'", ds_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate nested cache fields
            if key == "cache"
                && let Some(c_obj) = _value.as_object()
            {
                for c_key in c_obj.keys() {
                    if !VALID_CACHE_FIELDS.contains(&c_key.as_str()) {
                        let suggestion = find_closest_match(c_key, VALID_CACHE_FIELDS);
                        errors.push(ValidationError {
                            field: format!("cache.{}", c_key),
                            message: format!("Unknown cache field: '{}'", c_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate nested image fields
            if key == "image"
                && let Some(i_obj) = _value.as_object()
            {
                for i_key in i_obj.keys() {
                    if !VALID_IMAGE_FIELDS.contains(&i_key.as_str()) {
                        let suggestion = find_closest_match(i_key, VALID_IMAGE_FIELDS);
                        errors.push(ValidationError {
                            field: format!("image.{}", i_key),
                            message: format!("Unknown image field: '{}'", i_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate framework value
            if key == "framework"
                && let Some(fw) = _value.as_str()
                && !VALID_FRAMEWORKS.contains(&fw)
            {
                let suggestion = find_closest_match(fw, VALID_FRAMEWORKS);
                errors.push(ValidationError {
                    field: key.clone(),
                    message: format!("Invalid framework: '{}'", fw),
                    suggestion: suggestion.map(|s| {
                        format!(
                            "Did you mean '{}'? Valid: {}",
                            s,
                            VALID_FRAMEWORKS.join(", ")
                        )
                    }),
                });
            }

            // Validate outputFormat value
            if key == "outputFormat"
                && let Some(of) = _value.as_str()
                && !VALID_OUTPUT_FORMATS.contains(&of)
            {
                let suggestion = find_closest_match(of, VALID_OUTPUT_FORMATS);
                errors.push(ValidationError {
                    field: key.clone(),
                    message: format!("Invalid outputFormat: '{}'", of),
                    suggestion: suggestion.map(|s| {
                        format!(
                            "Did you mean '{}'? Valid: {}",
                            s,
                            VALID_OUTPUT_FORMATS.join(", ")
                        )
                    }),
                });
            }

            // Validate edgeTarget value
            if key == "edgeTarget"
                && let Some(et) = _value.as_str()
                && !VALID_EDGE_TARGETS.contains(&et)
            {
                let suggestion = find_closest_match(et, VALID_EDGE_TARGETS);
                errors.push(ValidationError {
                    field: key.clone(),
                    message: format!("Invalid edgeTarget: '{}'", et),
                    suggestion: suggestion.map(|s| {
                        format!(
                            "Did you mean '{}'? Valid: {}",
                            s,
                            VALID_EDGE_TARGETS.join(", ")
                        )
                    }),
                });
            }

            // Validate nested build fields
            if key == "build"
                && let Some(b_obj) = _value.as_object()
            {
                for b_key in b_obj.keys() {
                    if !VALID_BUILD_FIELDS.contains(&b_key.as_str()) {
                        let suggestion = find_closest_match(b_key, VALID_BUILD_FIELDS);
                        errors.push(ValidationError {
                            field: format!("build.{}", b_key),
                            message: format!("Unknown build field: '{}'", b_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
                // Validate preloadStrategy value
                if let Some(ps) = b_obj.get("preloadStrategy").and_then(|v| v.as_str())
                    && !VALID_PRELOAD_STRATEGIES.contains(&ps)
                {
                    let suggestion = find_closest_match(ps, VALID_PRELOAD_STRATEGIES);
                    errors.push(ValidationError {
                        field: "build.preloadStrategy".to_string(),
                        message: format!("Invalid preloadStrategy: '{}'", ps),
                        suggestion: suggestion.map(|s| {
                            format!(
                                "Did you mean '{}'? Valid: {}",
                                s,
                                VALID_PRELOAD_STRATEGIES.join(", ")
                            )
                        }),
                    });
                }
                // Validate wasmSimd value
                if let Some(ws) = b_obj.get("wasmSimd").and_then(|v| v.as_str())
                    && !VALID_WASM_SIMD_MODES.contains(&ws)
                {
                    let suggestion = find_closest_match(ws, VALID_WASM_SIMD_MODES);
                    errors.push(ValidationError {
                        field: "build.wasmSimd".to_string(),
                        message: format!("Invalid wasmSimd: '{}'", ws),
                        suggestion: suggestion.map(|s| {
                            format!(
                                "Did you mean '{}'? Valid: {}",
                                s,
                                VALID_WASM_SIMD_MODES.join(", ")
                            )
                        }),
                    });
                }
            }

            // Validate nested webhooks fields (#105)
            if key == "webhooks"
                && let Some(w_obj) = _value.as_object()
            {
                for w_key in w_obj.keys() {
                    if !VALID_WEBHOOK_FIELDS.contains(&w_key.as_str()) {
                        let suggestion = find_closest_match(w_key, VALID_WEBHOOK_FIELDS);
                        errors.push(ValidationError {
                            field: format!("webhooks.{}", w_key),
                            message: format!("Unknown webhooks field: '{}'", w_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate nested i18n fields (#106)
            if key == "i18n"
                && let Some(i_obj) = _value.as_object()
            {
                for i_key in i_obj.keys() {
                    if !VALID_I18N_FIELDS.contains(&i_key.as_str()) {
                        let suggestion = find_closest_match(i_key, VALID_I18N_FIELDS);
                        errors.push(ValidationError {
                            field: format!("i18n.{}", i_key),
                            message: format!("Unknown i18n field: '{}'", i_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate nested css fields (#107)
            if key == "css"
                && let Some(c_obj) = _value.as_object()
            {
                for c_key in c_obj.keys() {
                    if !VALID_CSS_FIELDS.contains(&c_key.as_str()) {
                        let suggestion = find_closest_match(c_key, VALID_CSS_FIELDS);
                        errors.push(ValidationError {
                            field: format!("css.{}", c_key),
                            message: format!("Unknown css field: '{}'", c_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
                // Validate rtl value
                if let Some(rtl) = c_obj.get("rtl").and_then(|v| v.as_str())
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
            }

            // Validate nested a11y fields (#108)
            if key == "a11y"
                && let Some(a_obj) = _value.as_object()
            {
                for a_key in a_obj.keys() {
                    if !VALID_A11Y_FIELDS.contains(&a_key.as_str()) {
                        let suggestion = find_closest_match(a_key, VALID_A11Y_FIELDS);
                        errors.push(ValidationError {
                            field: format!("a11y.{}", a_key),
                            message: format!("Unknown a11y field: '{}'", a_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate nested encrypt fields (#109)
            if key == "encrypt"
                && let Some(e_obj) = _value.as_object()
            {
                for e_key in e_obj.keys() {
                    if !VALID_ENCRYPT_FIELDS.contains(&e_key.as_str()) {
                        let suggestion = find_closest_match(e_key, VALID_ENCRYPT_FIELDS);
                        errors.push(ValidationError {
                            field: format!("encrypt.{}", e_key),
                            message: format!("Unknown encrypt field: '{}'", e_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }

            // Validate nested budgets fields (#102)
            if key == "budgets"
                && let Some(b_obj) = _value.as_object()
            {
                for b_key in b_obj.keys() {
                    if !VALID_BUDGET_FIELDS.contains(&b_key.as_str()) {
                        let suggestion = find_closest_match(b_key, VALID_BUDGET_FIELDS);
                        errors.push(ValidationError {
                            field: format!("budgets.{}", b_key),
                            message: format!("Unknown budgets field: '{}'", b_key),
                            suggestion: suggestion.map(|s| format!("Did you mean '{}'?", s)),
                        });
                    }
                }
            }
        }
    }

    errors
}

/// Validate config *values* (not just field names).
///
/// Checks ranges, cross-field requirements, and file existence for values
/// that cannot be caught by the field-name validation above.
pub fn validate_config_values(config: &serde_json::Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    if let Some(obj) = config.as_object() {
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

/// Find the closest matching string using Levenshtein distance.
pub fn find_closest_match(input: &str, candidates: &[&str]) -> Option<String> {
    let input_lower = input.to_lowercase();
    let mut best: Option<(usize, &str)> = None;

    for candidate in candidates {
        let dist = levenshtein(&input_lower, &candidate.to_lowercase());
        if best.is_none() || dist < best.unwrap().0 {
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
