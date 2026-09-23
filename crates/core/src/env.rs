// Environment variable loading and injection
//
// Loads .env files in order of precedence:
//   1. .env.[mode].local (e.g., .env.development.local)
//   2. .env.[mode]       (e.g., .env.development)
//   3. .env.local
//   4. .env
//
// Variables are injected into import.meta.env.* based on configured prefixes.

use crate::config::BuildMode;
use std::collections::HashMap;
use std::path::Path;

/// Loaded environment variables from .env files + process environment
pub struct EnvVars {
    vars: HashMap<String, String>,
}

impl EnvVars {
    /// Load environment variables from .env files in the project root.
    /// Mode determines which mode-specific files to load.
    pub fn load(root: &Path, mode: BuildMode, prefixes: &[String]) -> Self {
        let mode_str = match mode {
            BuildMode::Development => "development",
            BuildMode::Production => "production",
        };

        // Load in order of precedence (later files override earlier)
        let candidates = [
            root.join(".env"),
            root.join(".env.local"),
            root.join(format!(".env.{}", mode_str)),
            root.join(format!(".env.{}.local", mode_str)),
        ];

        let mut vars = HashMap::new();

        // Load .env files first (later files override earlier)
        for path in &candidates {
            if path.exists()
                && let Ok(content) = std::fs::read_to_string(path)
            {
                Self::parse_env_file(&content, &mut vars);
            }
        }

        // Process environment variables take precedence over .env files:
        // only insert process env values that are not already set, OR override
        // existing .env values so the real environment always wins.
        for (key, value) in std::env::vars() {
            if prefixes.iter().any(|p| key.starts_with(p)) {
                vars.insert(key, value);
            }
        }

        // Always inject built-in variables
        vars.insert(
            "PLEDGE_DEV".to_string(),
            match mode {
                BuildMode::Development => "true".to_string(),
                BuildMode::Production => "false".to_string(),
            },
        );
        vars.insert(
            "PLEDGE_PROD".to_string(),
            match mode {
                BuildMode::Development => "false".to_string(),
                BuildMode::Production => "true".to_string(),
            },
        );
        vars.insert("PLEDGE_MODE".to_string(), mode_str.to_string());

        EnvVars { vars }
    }

    /// Parse a .env file content into the vars map
    fn parse_env_file(content: &str, vars: &mut HashMap<String, String>) {
        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse KEY=VALUE
            if let Some(eq_pos) = line.find('=') {
                let key = line[..eq_pos].trim().to_string();
                let mut value = line[eq_pos + 1..].trim().to_string();

                // Remove surrounding quotes
                if (value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\''))
                {
                    value = value[1..value.len() - 1].to_string();
                }

                // Expand ${VAR} references
                value = Self::expand_vars(&value, vars);

                vars.insert(key, value);
            }
        }
    }

    /// Expand ${VAR} and $VAR references in a value
    fn expand_vars(value: &str, vars: &HashMap<String, String>) -> String {
        let mut result = value.to_string();

        // Expand ${VAR} patterns
        while let Some(start) = result.find("${") {
            if let Some(end) = result[start..].find('}') {
                let var_name = &result[start + 2..start + end];
                let env_val = std::env::var(var_name).ok();
                let replacement = vars
                    .get(var_name)
                    .map(|s| s.as_str())
                    .or(env_val.as_deref())
                    .unwrap_or("");
                result.replace_range(start..start + end + 1, replacement);
            } else {
                break;
            }
        }

        result
    }

    /// Get a variable value
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(|s| s.as_str())
    }

    /// Get all variables matching the given prefixes
    pub fn get_with_prefixes(&self, prefixes: &[String]) -> HashMap<String, String> {
        self.vars
            .iter()
            .filter(|(k, _)| prefixes.iter().any(|p| k.starts_with(p)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get all variables
    pub fn all(&self) -> &HashMap<String, String> {
        &self.vars
    }

    /// Replace import.meta.env.* references in code with actual values
    pub fn inject_into_code(&self, code: &str, prefixes: &[String]) -> String {
        let mut result = code.to_string();

        if !result.contains("import.meta.env") {
            return result;
        }

        // Replace import.meta.env.VAR_NAME with a string literal.
        //
        // Boundary-aware: `import.meta.env.PLEDGE_API` must not rewrite the
        // head of `import.meta.env.PLEDGE_API_KEY` — the char after the var
        // name must not be an identifier char. serde_json produces a fully
        // escaped JS string literal, so `\`/`"`/newlines/`${` in a .env value
        // can't break out of the string and inject code.
        let env_vars = self.get_with_prefixes(prefixes);
        for (key, value) in &env_vars {
            let pattern = format!("import.meta.env.{}", key);
            let replacement = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
            result = replace_member_access(&result, &pattern, &replacement);
        }

        // Replace built-in variables
        let dev = self.get("PLEDGE_DEV").unwrap_or("false") == "true";
        let mode = self.get("PLEDGE_MODE").unwrap_or("development");
        for (name, replacement) in [
            ("PLEDGE_DEV", if dev { "true" } else { "false" }),
            ("PLEDGE_PROD", if dev { "false" } else { "true" }),
            ("PLEDGE_MODE", ""),
            ("MODE", ""),
            ("DEV", if dev { "true" } else { "false" }),
            ("PROD", if dev { "false" } else { "true" }),
            ("SSR", "false"),
        ] {
            let lit = if replacement.is_empty() {
                serde_json::to_string(mode).unwrap_or_else(|_| "\"development\"".to_string())
            } else {
                replacement.to_string()
            };
            result = replace_member_access(&result, &format!("import.meta.env.{name}"), &lit);
        }

        result
    }

    /// Generate TypeScript declarations for import.meta.env
    pub fn generate_dts(&self, prefixes: &[String]) -> String {
        let env_vars = self.get_with_prefixes(prefixes);

        let mut entries: Vec<(String, String)> = env_vars
            .iter()
            .map(|(k, v)| {
                let ty = if v == "true" || v == "false" {
                    "boolean".to_string()
                } else if v.parse::<f64>().is_ok() {
                    "number".to_string()
                } else {
                    "string".to_string()
                };
                (k.clone(), ty)
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut props = String::new();
        for (key, ty) in &entries {
            props.push_str(&format!("    readonly {}: {};\n", key, ty));
        }

        // Built-ins — PLEDGE_DEV/PROD/MODE are injected into `vars` at load
        // time, so they already appear in `entries` when a `PLEDGE_` prefix is
        // configured; emitting them again would duplicate interface members.
        for (key, ty) in [
            ("PLEDGE_DEV", "boolean"),
            ("PLEDGE_PROD", "boolean"),
            ("PLEDGE_MODE", "string"),
            ("MODE", "string"),
            ("DEV", "boolean"),
            ("PROD", "boolean"),
            ("SSR", "boolean"),
        ] {
            if !entries.iter().any(|(k, _)| k == key) {
                props.push_str(&format!("    readonly {}: {};\n", key, ty));
            }
        }

        format!(
            r#"/// <reference types="pledge/client" />

interface ImportMetaEnv {{
{}
}}

interface ImportMeta {{
    readonly env: ImportMetaEnv;
}}
"#,
            props
        )
    }
}

/// Replace `pattern` with `replacement` only where it is not followed by an
/// identifier continuation char (`[A-Za-z0-9_$]`). A plain `str::replace` on
/// `import.meta.env.FOO` would corrupt `import.meta.env.FOO_BAR` into
/// `"value"_BAR` — emitting broken JS and potentially injecting a *different*
/// variable's value into a place the developer never asked for it.
fn replace_member_access(code: &str, pattern: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut rest = code;
    while let Some(pos) = rest.find(pattern) {
        let end = pos + pattern.len();
        let follows_ident = rest[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        out.push_str(&rest[..pos]);
        if follows_ident {
            out.push_str(&rest[pos..end]);
        } else {
            out.push_str(replacement);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &[(&str, &str)]) -> EnvVars {
        EnvVars {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn env_value_with_trailing_backslash_cannot_inject() {
        // `KEY=abc\` used to emit `"abc\"` — the backslash escaped the closing
        // quote and whatever followed was interpreted as live JS.
        let env = env_with(&[("PLEDGE_X", "abc\\")]);
        let out = env.inject_into_code("const v = import.meta.env.PLEDGE_X;", &["PLEDGE_".into()]);
        assert_eq!(out, "const v = \"abc\\\\\";");
    }

    #[test]
    fn env_value_with_quote_escape_sequence_cannot_inject() {
        let env = env_with(&[("PLEDGE_X", "\\\"; alert(1); //")]);
        let out = env.inject_into_code("const v = import.meta.env.PLEDGE_X;", &["PLEDGE_".into()]);
        // Both the backslash and the quote are escaped — the value can never
        // terminate the string literal it lands in.
        assert_eq!(out, "const v = \"\\\\\\\"; alert(1); //\";");
    }

    #[test]
    fn env_value_with_newline_stays_inside_string() {
        let env = env_with(&[("PLEDGE_X", "line1\nline2")]);
        let out = env.inject_into_code("import.meta.env.PLEDGE_X", &["PLEDGE_".into()]);
        assert_eq!(out, "\"line1\\nline2\"");
    }

    #[test]
    fn longer_var_name_is_not_rewritten_by_shorter_prefix() {
        let env = env_with(&[("PLEDGE_API", "short")]);
        let out = env.inject_into_code(
            "import.meta.env.PLEDGE_API_KEY; import.meta.env.PLEDGE_API;",
            &["PLEDGE_".into()],
        );
        assert_eq!(
            out,
            "import.meta.env.PLEDGE_API_KEY; \"short\";",
            "got: {out}"
        );
    }

    #[test]
    fn builtin_dev_does_not_rewrite_device() {
        let env = env_with(&[]);
        let out = env.inject_into_code("import.meta.env.DEVICE; import.meta.env.DEV;", &[]);
        assert!(out.contains("import.meta.env.DEVICE"));
        assert!(out.contains("; false;") || out.contains("; true;"));
    }
}
