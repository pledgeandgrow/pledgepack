// Compression output — generate .gz and .br compressed files
//
// During production builds, generates pre-compressed versions of all output files:
//   - .gz files using gzip compression
//   - .br files using Brotli compression
//
// These can be served directly by web servers with proper Content-Encoding headers,
// avoiding on-the-fly compression overhead.

use anyhow::Result;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tracing::info;

/// Compress all files in a directory using gzip (.gz) and/or Brotli (.br)
pub fn compress_directory(dir: &Path, gzip: bool, brotli: bool) -> Result<CompressionStats> {
    let mut stats = CompressionStats::default();

    let mut files_to_compress = Vec::new();

    // Collect all files that should be compressed
    collect_files(dir, &mut files_to_compress)?;

    for file_path in &files_to_compress {
        let original_size = std::fs::metadata(file_path)?.len() as usize;

        if gzip {
            let gz_path = format!("{}.gz", file_path.to_string_lossy());
            match compress_gzip(file_path, &gz_path) {
                Ok(gz_size) => {
                    stats.files_compressed += 1;
                    stats.original_bytes += original_size;
                    stats.gzipped_bytes += gz_size;
                }
                Err(e) => {
                    tracing::warn!("Failed to gzip {}: {}", crate::display_path(&file_path), e);
                }
            }
        }

        if brotli {
            let br_path = format!("{}.br", file_path.to_string_lossy());
            match compress_brotli(file_path, &br_path) {
                Ok(br_size) => {
                    if !gzip {
                        stats.files_compressed += 1;
                        stats.original_bytes += original_size;
                    }
                    stats.brotli_bytes += br_size;
                }
                Err(e) => {
                    tracing::warn!("Failed to brotli {}: {}", crate::display_path(&file_path), e);
                }
            }
        }
    }

    info!(
        "Compressed {} files: {} → {} (gzip), {} (brotli)",
        stats.files_compressed,
        format_bytes(stats.original_bytes),
        format_bytes(stats.gzipped_bytes),
        format_bytes(stats.brotli_bytes),
    );

    Ok(stats)
}

/// Compression statistics
#[derive(Debug, Default)]
pub struct CompressionStats {
    pub files_compressed: usize,
    pub original_bytes: usize,
    pub gzipped_bytes: usize,
    pub brotli_bytes: usize,
}

impl CompressionStats {
    /// Gzip compression ratio (0.0 - 1.0)
    pub fn gzip_ratio(&self) -> f64 {
        if self.original_bytes == 0 {
            return 0.0;
        }
        self.gzipped_bytes as f64 / self.original_bytes as f64
    }

    /// Brotli compression ratio (0.0 - 1.0)
    pub fn brotli_ratio(&self) -> f64 {
        if self.original_bytes == 0 {
            return 0.0;
        }
        self.brotli_bytes as f64 / self.original_bytes as f64
    }

    /// Format compression stats as a comfy-table for CLI output
    pub fn format_stats_table(&self) -> String {
        let mut table = comfy_table::Table::new();
        table
            .load_preset(comfy_table::presets::UTF8_FULL)
            .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
            .set_content_arrangement(comfy_table::ContentArrangement::Dynamic)
            .set_header(vec!["Metric", "Value"])
            .add_row(vec!["Files Compressed", &self.files_compressed.to_string()])
            .add_row(vec!["Original Size", &format_bytes(self.original_bytes)])
            .add_row(vec!["Gzip Size", &format_bytes(self.gzipped_bytes)])
            .add_row(vec![
                "Gzip Ratio",
                &format!("{:.1}%", self.gzip_ratio() * 100.0),
            ])
            .add_row(vec!["Brotli Size", &format_bytes(self.brotli_bytes)])
            .add_row(vec![
                "Brotli Ratio",
                &format!("{:.1}%", self.brotli_ratio() * 100.0),
            ]);
        table.to_string()
    }
}

/// Collect files that should be compressed (JS, CSS, HTML, JSON, SVG)
fn collect_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            collect_files(&path, files)?;
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "js" | "mjs" | "css" | "html" | "json" | "svg" | "wasm") {
                // Skip already-compressed files
                if !path.to_string_lossy().ends_with(".gz")
                    && !path.to_string_lossy().ends_with(".br")
                {
                    files.push(path);
                }
            }
        }
    }

    Ok(())
}

/// Compress a file using gzip (real flate2 implementation)
fn compress_gzip(input: &Path, output: &str) -> Result<usize> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::BufReader;

    let input_file = File::open(input)?;
    let output_file = File::create(output)?;
    let mut encoder = GzEncoder::new(output_file, Compression::default());
    let mut reader = BufReader::new(input_file);
    std::io::copy(&mut reader, &mut encoder)?;
    let output_file = encoder.finish()?;
    Ok(output_file.metadata()?.len() as usize)
}

/// Compress a file using Brotli (real brotli crate implementation)
fn compress_brotli(input: &Path, output: &str) -> Result<usize> {
    use brotli::CompressorReader;
    use std::io::Read;

    let mut input_file = File::open(input)?;
    let mut data = Vec::new();
    input_file.read_to_end(&mut data)?;

    let mut compressed = Vec::new();
    let mut reader = CompressorReader::new(&data[..], 4096, 11, 22);
    reader.read_to_end(&mut compressed)?;

    let mut output_file = File::create(output)?;
    output_file.write_all(&compressed)?;
    Ok(compressed.len())
}

/// Format bytes as a human-readable string
fn format_bytes(bytes: usize) -> String {
    crate::format_size(bytes)
}
