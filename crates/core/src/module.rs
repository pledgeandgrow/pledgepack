use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Unique identifier for a module in the graph
pub type ModuleId = u32;

/// Type of a resolved module
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModuleKind {
    JavaScript,
    TypeScript,
    Jsx,
    Tsx,
    Css,
    Json,
    Asset,
    Wasm,
    Vue,
    Svelte,
    Astro,
    Worker,
    SharedWorker,
    WebComponent,
    Mdx,
    Graphql,
    Yaml,
    Csv,
    Tsv,
    Sass,
    Toml,
    Shader,
    /// PledgeStack PSX file — Rust + JSX hybrid (.psx)
    Psx,
    /// PledgeStack PS file — pure Rust server module (.ps)
    Ps,
    Unknown,
}

impl ModuleKind {
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_ascii_lowercase().as_str() {
            ".tsx" => Self::Tsx,
            ".ts" => Self::TypeScript,
            ".jsx" => Self::Jsx,
            ".js" | ".mjs" | ".cjs" => Self::JavaScript,
            ".css" => Self::Css,
            ".json" => Self::Json,
            ".wasm" => Self::Wasm,
            ".vue" => Self::Vue,
            ".svelte" => Self::Svelte,
            ".astro" => Self::Astro,
            ".worker.js" | ".worker.ts" => Self::Worker,
            ".wc.tsx" | ".wc.jsx" => Self::WebComponent,
            ".mdx" => Self::Mdx,
            ".graphql" | ".gql" => Self::Graphql,
            ".yaml" | ".yml" => Self::Yaml,
            ".csv" => Self::Csv,
            ".tsv" => Self::Tsv,
            ".scss" | ".sass" => Self::Sass,
            ".toml" => Self::Toml,
            ".glsl" | ".frag" | ".vert" | ".comp" | ".wgsl" => Self::Shader,
            ".psx" => Self::Psx,
            ".ps" => Self::Ps,
            ".png" | ".jpg" | ".jpeg" | ".gif" | ".svg" | ".webp" | ".ico" | ".woff" | ".woff2"
            | ".ttf" | ".otf" | ".eot" | ".mp4" | ".webm" | ".mp3" | ".wav" | ".pdf" => Self::Asset,
            _ => Self::Unknown,
        }
    }

    /// Classify a file by its full name. Unlike [`from_extension`](Self::from_extension)
    /// (which only ever sees the last extension, so the compound `.worker.js` /
    /// `.worker.ts` arms can never match there), this recognises
    /// `*.worker.js` / `*.worker.ts` files as [`ModuleKind::Worker`].
    pub fn from_path(path: &std::path::Path) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if name.ends_with(".worker.js") || name.ends_with(".worker.ts") {
            return Self::Worker;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        Self::from_extension(&ext)
    }

    pub fn is_typescript(&self) -> bool {
        matches!(self, Self::TypeScript | Self::Tsx | Self::Psx)
    }

    pub fn is_jsx(&self) -> bool {
        matches!(self, Self::Jsx | Self::Tsx | Self::Psx)
    }

    /// Returns true if this module type is a PledgeStack-specific format (PSX or PS)
    pub fn is_pledgestack(&self) -> bool {
        matches!(self, Self::Psx | Self::Ps)
    }
}

/// A fully resolved module — ready to be parsed and transformed
#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub id: ModuleId,
    pub path: PathBuf,
    pub kind: ModuleKind,
    /// Raw source content (read from filesystem)
    pub source: Vec<u8>,
    /// Content hash for cache invalidation
    pub content_hash: u64,
}

impl ResolvedModule {
    pub fn extension(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{}", e))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_path_recognises_worker_files() {
        use std::path::Path;
        assert_eq!(
            ModuleKind::from_path(Path::new("src/a.worker.ts")),
            ModuleKind::Worker
        );
        assert_eq!(
            ModuleKind::from_path(Path::new("src/A.Worker.JS")),
            ModuleKind::Worker
        );
        assert_eq!(
            ModuleKind::from_path(Path::new("src/a.ts")),
            ModuleKind::TypeScript
        );
        assert_eq!(
            ModuleKind::from_path(Path::new("src/workerish.js")),
            ModuleKind::JavaScript
        );
        // The old extension-only lookup can never see a compound extension.
        assert_eq!(ModuleKind::from_extension(".ts"), ModuleKind::TypeScript);
    }

    #[test]
    fn from_extension_is_case_insensitive() {
        assert_eq!(ModuleKind::from_extension(".PNG"), ModuleKind::Asset);
        assert_eq!(ModuleKind::from_extension(".TSX"), ModuleKind::Tsx);
        assert_eq!(ModuleKind::from_extension(".Css"), ModuleKind::Css);
    }
}
