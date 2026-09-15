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

        // Replace import.meta.env.VAR_NAME with string literal
        let env_vars = self.get_with_prefixes(prefixes);
        for (key, value) in &env_vars {
            let pattern = format!("import.meta.env.{}", key);
            let replacement = format!("\"{}\"", value.replace('"', "\\\""));
            result = result.replace(&pattern, &replacement);
        }

        // Replace built-in variables
        result = result.replace(
            "import.meta.env.PLEDGE_DEV",
            if self.get("PLEDGE_DEV").unwrap_or("false") == "true" {
                "true"
            } else {
                "false"
            },
        );
        result = result.replace(
            "import.meta.env.PLEDGE_PROD",
            if self.get("PLEDGE_PROD").unwrap_or("false") == "true" {
                "true"
            } else {
                "false"
            },
        );
        if let Some(mode) = self.get("PLEDGE_MODE") {
            result = result.replace("import.meta.env.PLEDGE_MODE", &format!("\"{}\"", mode));
        }
        result = result.replace(
            "import.meta.env.MODE",
            &format!("\"{}\"", self.get("PLEDGE_MODE").unwrap_or("development")),
        );
        result = result.replace(
            "import.meta.env.DEV",
            if self.get("PLEDGE_DEV").unwrap_or("false") == "true" {
                "true"
            } else {
                "false"
            },
        );
        result = result.replace(
            "import.meta.env.PROD",
            if self.get("PLEDGE_PROD").unwrap_or("false") == "true" {
                "true"
            } else {
                "false"
            },
        );

        // Replace import.meta.env.SSR with false (no SSR by default)
        result = result.replace("import.meta.env.SSR", "false");

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

        // Add built-in
        props.push_str("    readonly PLEDGE_DEV: boolean;\n");
        props.push_str("    readonly PLEDGE_PROD: boolean;\n");
        props.push_str("    readonly PLEDGE_MODE: string;\n");
        props.push_str("    readonly MODE: string;\n");
        props.push_str("    readonly DEV: boolean;\n");
        props.push_str("    readonly PROD: boolean;\n");
        props.push_str("    readonly SSR: boolean;\n");

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
