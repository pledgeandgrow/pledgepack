// Asset transforms: JSON, static assets, WASM, shaders

use super::TransformOutput;
use crate::config::PledgeConfig;
use anyhow::Result;
use std::path::Path;

/// Transform JSON into an ES module with named exports
/// Supports both default export and named exports for top-level keys
/// In production mode, JSON is minified (compact serialization)
pub(super) fn transform_json(source: &str) -> Result<TransformOutput> {
    let value: serde_json::Value =
        serde_json::from_str(source).map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;

    let mut code = String::new();

    if let serde_json::Value::Object(map) = &value {
        for (key, val) in map {
            // Only generate a named export if the key is a valid JS identifier.
            // Keys with hyphens (e.g. "my-key"), dots (e.g. "nested.key"), or
            // other special characters are NOT valid JS identifiers, so they
            // are skipped for named exports — but they remain fully accessible
            // via the default export below (which serializes the entire object).
            if key
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                && !key.chars().next().map(|c| c.is_numeric()).unwrap_or(true)
            {
                let val_str = serde_json::to_string(val).unwrap_or_else(|_| "null".to_string());
                code.push_str(&format!("export const {} = {};\n", key, val_str));
            }
        }
    }

    let default_export =
        serde_json::to_string(&value).unwrap_or_else(|_| source.trim().to_string());
    code.push_str(&format!("export default {};", default_export));

    Ok(TransformOutput {
        code,
        source_map: None,
        css_modules: None,
        is_css: false,
        extracted_css: None,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}

/// Transform static asset imports into URL strings
/// import logo from './logo.png' → export default "/src/logo.png"
/// With ?inline query → base64 data URI
/// In production, assets smaller than assets_inline_limit are automatically inlined as base64
/// When image optimization is enabled, raster images are processed: resized, converted to WebP/JPEG, with srcset and blur placeholder
pub(super) fn transform_asset(
    file_path: &str,
    source: &[u8],
    is_production: bool,
    config: &PledgeConfig,
) -> Result<TransformOutput> {
    let is_inline = file_path.contains("?inline")
        || (is_production && source.len() < config.build.assets_inline_limit);
    let clean_path = file_path.split('?').next().unwrap_or(file_path);

    // SVG assets get dedicated handling regardless of image optimization
    // settings: SVGO-style optimization in production, `?sprite` imports in
    // all modes, and the proper image/svg+xml content type when inlined.
    if crate::svg::is_svg(std::path::Path::new(clean_path)) {
        let svg_source = std::str::from_utf8(source).unwrap_or("");
        let optimize = is_production && config.image.enabled;
        let optimized = if optimize {
            crate::svg::optimize_svg(svg_source, &crate::svg::SvgOptions::default())
        } else {
            svg_source.to_string()
        };

        if file_path.contains("?sprite") {
            let sprite_id = std::path::Path::new(clean_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("icon");
            let sprite_entry = crate::svg::SvgSpriteEntry {
                id: sprite_id.to_string(),
                svg: optimized.clone(),
            };
            let sprite = crate::svg::generate_sprite(&[sprite_entry]);
            let url = format!("/{}", crate::normalize_path_str(clean_path));
            let code = format!(
                r#"export default "{}";
export const sprite = `{}`;"#,
                url, sprite
            );
            return Ok(TransformOutput {
                code,
                source_map: None,
                css_modules: None,
                is_css: false,
                extracted_css: Some(sprite),
                is_worker: false,
                dynamic_imports: Vec::new(),
                content_hash: None,
            });
        }

        if is_inline {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(optimized.as_bytes());
            let code = format!("export default \"data:image/svg+xml;base64,{}\";", b64);
            return Ok(TransformOutput {
                code,
                source_map: None,
                css_modules: None,
                is_css: false,
                extracted_css: None,
                is_worker: false,
                dynamic_imports: Vec::new(),
                content_hash: None,
            });
        }

        let url = format!("/{}", crate::normalize_path_str(clean_path));
        let code = format!("export default \"{}\";", url);
        return Ok(TransformOutput {
            code,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: if optimize { Some(optimized) } else { None },
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        });
    }

    if is_production && config.image.enabled && !is_inline {
        if crate::image_pipeline::is_raster_image(source) {
            use crate::image_pipeline::{
                ImageFormat, ImageOptions, generate_image_module, process_image,
            };

            let mut formats = Vec::new();
            if config.image.webp {
                formats.push(ImageFormat::WebP);
            }
            if config.image.avif {
                formats.push(ImageFormat::AVIF);
            }
            formats.push(ImageFormat::JPEG);

            let opts = ImageOptions {
                formats,
                widths: if !config.image.responsive_widths.is_empty() {
                    config.image.responsive_widths.clone()
                } else {
                    vec![640, 750, 828, 1080, 1200, 1920, 2048]
                },
                quality: config.image.quality as u8,
                blur_placeholder: true,
                progressive: true,
                strip_metadata: true,
            };

            match process_image(source, clean_path, &opts) {
                Ok(processed) => {
                    let code = generate_image_module(&processed);
                    return Ok(TransformOutput {
                        code,
                        source_map: None,
                        css_modules: None,
                        is_css: false,
                        extracted_css: None,
                        is_worker: false,
                        dynamic_imports: Vec::new(),
                        content_hash: None,
                    });
                }
                Err(e) => {
                    tracing::warn!("Image optimization failed for {}: {}", clean_path, e);
                }
            }
        }
    }

    if crate::asset_pipeline::is_audio_file(clean_path) {
        let code = crate::asset_pipeline::transform_audio_asset(clean_path, is_inline, source);
        return Ok(TransformOutput {
            code,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        });
    }
    if crate::asset_pipeline::is_video_file(clean_path) {
        let code = crate::asset_pipeline::transform_video_asset(clean_path, is_inline, source);
        return Ok(TransformOutput {
            code,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        });
    }
    if clean_path.ends_with(".pdf") {
        let code = crate::asset_pipeline::transform_pdf_asset(clean_path, is_inline, source);
        return Ok(TransformOutput {
            code,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        });
    }

    if is_inline {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(source);
        let mime = guess_mime(clean_path);
        let data_uri = format!("data:{};base64,{}", mime, b64);
        let code = format!("export default \"{}\";", data_uri);
        Ok(TransformOutput {
            code,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        })
    } else {
        let url = format!("/{}", crate::normalize_path_str(clean_path));
        let code = format!("export default \"{}\";", url);
        Ok(TransformOutput {
            code,
            source_map: None,
            css_modules: None,
            is_css: false,
            extracted_css: None,
            is_worker: false,
            dynamic_imports: Vec::new(),
            content_hash: None,
        })
    }
}

/// Transform WASM imports into async instantiation
/// import wasm from './module.wasm' → export default async function() { ... }
/// Supports SIMD auto-detection (#55): generates runtime feature detection
/// that uses WebAssembly.validate() to check for SIMD support, then loads
/// the appropriate WASM module variant.
pub(super) fn transform_wasm(file_path: &str, config: &PledgeConfig) -> Result<TransformOutput> {
    let url = format!("/{}", crate::normalize_path_str(file_path));
    let simd_mode = &config.build.wasm_simd;

    let code = match simd_mode.as_str() {
        "always" => {
            format!(
                r#"export default async function() {{
  const {{ instance }} = await WebAssembly.instantiateStreaming(
    fetch("{}", {{ headers: {{ "Content-Type": "application/wasm" }} }}),
    {{}}
  );
  return instance.exports;
}}"#,
                url
            )
        }
        "never" => crate::performance::generate_wasm_streaming_code(&url, true),
        _ => {
            let simd_url = format!("{}.simd.wasm", url.trim_end_matches(".wasm"));
            format!(
                r#"// WASM SIMD auto-detection (#55) + streaming compilation (#74)
const _simdTest = new Uint8Array([0,97,115,109,1,0,0,0,1,5,1,96,0,1,123,3,2,1,0,10,12,1,10,0,65,0,253,15,253,15,11]);
const _hasSimd = (() => {{
  try {{ return WebAssembly.validate(_simdTest); }} catch {{ return false; }}
}})();
export default async function() {{
  const url = _hasSimd ? "{}" : "{}";
  if (typeof WebAssembly.instantiateStreaming === 'function') {{
    const {{ instance }} = await WebAssembly.instantiateStreaming(
      fetch(url, {{ headers: {{ "Content-Type": "application/wasm" }} }}),
      {{}}
    );
    return instance.exports;
  }}
  const response = await fetch(url);
  const bytes = new Uint8Array(await response.arrayBuffer());
  const {{ instance }} = await WebAssembly.instantiate(bytes, {{}});
  return instance.exports;
}}"#,
                simd_url, url
            )
        }
    };

    Ok(TransformOutput {
        code,
        source_map: None,
        css_modules: None,
        is_css: false,
        extracted_css: None,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}

/// Guess MIME type from file extension
fn guess_mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// #63: Transform GLSL/WGSL shader files into ES module string exports
/// Supports .glsl, .vert, .frag, .comp (GLSL) and .wgsl (WebGPU Shading Language)
/// Exports the shader source as a default string, plus named exports for each
/// #pragma shader-stage section if present.
pub(super) fn transform_shader(source: &str, file_path: &str) -> Result<TransformOutput> {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("glsl");

    let shader_type = match ext {
        "vert" => "vertex",
        "frag" => "fragment",
        "comp" => "compute",
        "wgsl" => "wgsl",
        _ => "glsl",
    };

    let escaped = source
        .replace('\\', "\\\\")
        .replace('`', "\\`")
        .replace("${", "\\${");

    let code = format!(
        r#"const shader = `{}`;
const shaderType = "{}";
const shaderSource = `{}`;
export {{ shader, shaderType, shaderSource }};
export default shader;"#,
        escaped, shader_type, escaped
    );

    Ok(TransformOutput {
        code,
        source_map: None,
        css_modules: None,
        is_css: false,
        extracted_css: None,
        is_worker: false,
        dynamic_imports: Vec::new(),
        content_hash: None,
    })
}
