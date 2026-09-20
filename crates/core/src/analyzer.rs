// Build analyzer — bundle size analysis and visualization
//
// Generates a JSON analysis of the production bundle, including:
//   - Per-module sizes (original + transformed)
//   - Chunk breakdown
//   - Dependency tree with sizes
//   - Duplicate module detection
//
// The output can be viewed with `pledge analyze` which serves an interactive
// treemap visualization in the browser.

use crate::engine::BuildEngine;
use crate::module::ModuleKind;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use tracing::info;

/// Analysis result for a single module
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleAnalysis {
    pub path: String,
    pub kind: String,
    /// Source size as written on disk ("raw").
    pub original_size: usize,
    /// Size of the module's emitted (minified) code.
    pub transformed_size: usize,
    /// Gzip of the emitted (minified) code.
    #[serde(default)]
    pub gzip_size: usize,
    pub dependencies: Vec<String>,
    pub is_entry: bool,
    pub is_css: bool,
    pub is_worker: bool,
}

/// Full bundle analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAnalysis {
    pub total_modules: usize,
    /// Sum of raw source sizes.
    pub total_original_size: usize,
    /// Sum of emitted (minified) code sizes.
    pub total_transformed_size: usize,
    /// Sum of gzip sizes of the emitted code.
    #[serde(default)]
    pub total_gzip_size: usize,
    pub modules: Vec<ModuleAnalysis>,
    pub chunks: Vec<ChunkAnalysis>,
    pub duplicates: Vec<DuplicateModule>,
    pub largest_modules: Vec<ModuleAnalysis>,
}

/// Chunk analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkAnalysis {
    pub name: String,
    pub size: usize,
    pub module_count: usize,
    pub modules: Vec<String>,
}

/// Duplicate module detection
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateModule {
    pub specifier: String,
    pub paths: Vec<String>,
    pub total_size: usize,
}

/// Format bundle analysis as a comfy-table for CLI output
pub fn format_analysis_table(analysis: &BundleAnalysis) -> String {
    let mut summary = comfy_table::Table::new();
    summary
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic)
        .set_header(vec!["Metric", "Value"])
        .add_row(vec![
            "Total Size (minified)",
            &format_bytes(analysis.total_transformed_size),
        ])
        .add_row(vec![
            "Total Size (gzip)",
            &format_bytes(analysis.total_gzip_size),
        ])
        .add_row(vec![
            "Source Size (raw)",
            &format_bytes(analysis.total_original_size),
        ])
        .add_row(vec!["Total Modules", &analysis.total_modules.to_string()])
        .add_row(vec!["Chunks", &analysis.chunks.len().to_string()])
        .add_row(vec!["Duplicates", &analysis.duplicates.len().to_string()]);

    let mut modules = comfy_table::Table::new();
    modules
        .load_preset(comfy_table::presets::UTF8_FULL)
        .apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS)
        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic)
        .set_header(vec!["Path", "Type", "Minified", "Gzip", "%"]);

    let total = analysis.total_transformed_size.max(1) as f64;
    for m in analysis.largest_modules.iter().take(20) {
        let pct = (m.transformed_size as f64 / total) * 100.0;
        modules.add_row(vec![
            &m.path,
            &m.kind,
            &format_bytes(m.transformed_size),
            &format_bytes(m.gzip_size),
            &format!("{:.1}%", pct),
        ]);
    }

    format!("{}\n\n{}", summary, modules)
}

/// Gzip-compressed length of `bytes` (default compression level).
fn gzip_len(bytes: &[u8]) -> usize {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    if enc.write_all(bytes).is_err() {
        return 0;
    }
    enc.finish().map(|v| v.len()).unwrap_or(0)
}

/// Build the project the way `pledge analyze` and `pledge why` both need it
/// and analyze the result. Always a production build so the reported sizes
/// are those of the minified output regardless of the configured mode (a
/// development build is unminified and reports much larger numbers).
pub async fn build_and_analyze(
    config: &crate::config::PledgeConfig,
) -> Result<(BuildEngine, BundleAnalysis)> {
    let mut config = config.clone();
    config.mode = crate::config::BuildMode::Production;
    let mut engine = BuildEngine::new(std::sync::Arc::new(config));
    let _ = engine.build().await?;
    let analysis = analyze_build(&engine)?;
    Ok((engine, analysis))
}

/// One-line, explicitly labelled size summary shared by `pledge analyze` and
/// `pledge why` (raw source vs. minified output vs. gzip of the output).
pub fn format_size_summary(analysis: &BundleAnalysis) -> String {
    format!(
        "{} modules - source (raw) {}, minified {}, gzip {}",
        analysis.total_modules,
        format_bytes(analysis.total_original_size),
        format_bytes(analysis.total_transformed_size),
        format_bytes(analysis.total_gzip_size),
    )
}

/// Labelled sizes of one module: raw source, minified output, gzip.
pub fn format_module_sizes(m: &ModuleAnalysis) -> String {
    format!(
        "raw {}, minified {}, gzip {}",
        format_bytes(m.original_size),
        format_bytes(m.transformed_size),
        format_bytes(m.gzip_size),
    )
}

/// Analyze the build and generate a bundle analysis
pub fn analyze_build(engine: &BuildEngine) -> Result<BundleAnalysis> {
    let modules = engine.modules();
    let function_cache = engine.function_cache();

    let mut module_analyses: Vec<ModuleAnalysis> = Vec::new();
    let mut total_original = 0usize;
    let mut total_transformed = 0usize;
    let mut total_gzip = 0usize;

    for (id, module) in modules {
        let rel_path = crate::normalize_path(&module.path);
        let original_size = module.source.len();
        let transformed_size = function_cache
            .get(&crate::engine::module_cache_key(
                module.content_hash,
                &module.path,
            ))
            .map(|c| c.code.len())
            .unwrap_or(0);

        let cached = function_cache.get(&crate::engine::module_cache_key(
            module.content_hash,
            &module.path,
        ));

        let kind_str = match module.kind {
            ModuleKind::Tsx => "tsx",
            ModuleKind::TypeScript => "ts",
            ModuleKind::Jsx => "jsx",
            ModuleKind::JavaScript => "js",
            ModuleKind::Css => "css",
            ModuleKind::Json => "json",
            ModuleKind::Asset => "asset",
            ModuleKind::Wasm => "wasm",
            ModuleKind::Vue => "vue",
            ModuleKind::Svelte => "svelte",
            ModuleKind::Astro => "astro",
            ModuleKind::Worker => "worker",
            ModuleKind::Psx => "psx",
            ModuleKind::Ps => "ps",
            _ => "unknown",
        };

        let deps = cached.as_ref().map(|c| c.deps.clone()).unwrap_or_default();

        let is_css = cached.as_ref().map(|c| c.is_css).unwrap_or(false);
        let is_worker = cached.as_ref().map(|c| c.is_worker).unwrap_or(false);

        let gzip_size = cached
            .as_ref()
            .map(|c| gzip_len(c.code.as_bytes()))
            .unwrap_or(0);

        total_original += original_size;
        total_transformed += transformed_size;
        total_gzip += gzip_size;

        module_analyses.push(ModuleAnalysis {
            path: rel_path,
            kind: kind_str.to_string(),
            original_size,
            transformed_size,
            gzip_size,
            dependencies: deps,
            is_entry: engine.modules().values().take(1).any(|m| m.id == *id),
            is_css,
            is_worker,
        });
    }

    // Sort by size (largest first)
    let mut largest = module_analyses.clone();
    largest.sort_by_key(|a| std::cmp::Reverse(a.transformed_size));
    largest.truncate(20);

    // Detect duplicates (same specifier resolved from different paths)
    let duplicates = detect_duplicates(&module_analyses);

    // Generate chunk analysis (simplified — group by directory)
    let chunks = generate_chunk_analysis(&module_analyses);

    info!(
        "Bundle analysis: {} modules, {} total ({} → {})",
        module_analyses.len(),
        format_bytes(total_transformed),
        format_bytes(total_original),
        format_bytes(total_transformed),
    );

    Ok(BundleAnalysis {
        total_modules: module_analyses.len(),
        total_original_size: total_original,
        total_transformed_size: total_transformed,
        total_gzip_size: total_gzip,
        modules: module_analyses,
        chunks,
        duplicates,
        largest_modules: largest,
    })
}

/// Detect duplicate modules (same basename, different paths)
fn detect_duplicates(modules: &[ModuleAnalysis]) -> Vec<DuplicateModule> {
    let mut by_name: HashMap<String, Vec<&ModuleAnalysis>> = HashMap::new();

    for m in modules {
        let name = m.path.rsplit('/').next().unwrap_or(&m.path).to_string();
        by_name.entry(name).or_default().push(m);
    }

    let mut duplicates = Vec::new();
    for (name, entries) in &by_name {
        if entries.len() > 1 {
            let total_size: usize = entries.iter().map(|e| e.transformed_size).sum();
            duplicates.push(DuplicateModule {
                specifier: name.clone(),
                paths: entries.iter().map(|e| e.path.clone()).collect(),
                total_size,
            });
        }
    }

    duplicates.sort_by_key(|a| std::cmp::Reverse(a.total_size));
    duplicates
}

/// Generate chunk analysis by grouping modules by directory
fn generate_chunk_analysis(modules: &[ModuleAnalysis]) -> Vec<ChunkAnalysis> {
    let mut by_dir: HashMap<String, Vec<&ModuleAnalysis>> = HashMap::new();

    for m in modules {
        let dir = m
            .path
            .rsplit_once('/')
            .map(|x| x.0)
            .unwrap_or("")
            .to_string();
        by_dir.entry(dir).or_default().push(m);
    }

    let mut chunks: Vec<ChunkAnalysis> = by_dir
        .into_iter()
        .map(|(name, mods)| {
            let size: usize = mods.iter().map(|m| m.transformed_size).sum();
            ChunkAnalysis {
                name,
                size,
                module_count: mods.len(),
                modules: mods.iter().map(|m| m.path.clone()).collect(),
            }
        })
        .collect();

    chunks.sort_by_key(|a| std::cmp::Reverse(a.size));
    chunks
}

/// Generate the HTML visualization for the bundle analysis
pub fn generate_analysis_html(analysis: &BundleAnalysis) -> String {
    let total_kb = analysis.total_transformed_size as f64 / 1024.0;
    let original_kb = analysis.total_original_size as f64 / 1024.0;

    let module_rows: String = analysis
        .largest_modules
        .iter()
        .map(|m| {
            let size_kb = m.transformed_size as f64 / 1024.0;
            let pct = if analysis.total_transformed_size > 0 {
                (m.transformed_size as f64 / analysis.total_transformed_size as f64) * 100.0
            } else {
                0.0
            };
            format!(
                r#"<tr><td style="padding:4px 12px;color:#e0e0e0;">{}</td>
                <td style="padding:4px 12px;color:#888;">{}</td>
                <td style="padding:4px 12px;text-align:right;color:#e0e0e0;">{:.1}KB</td>
                <td style="padding:4px 12px;text-align:right;color:#666;">{:.1}%</td></tr>"#,
                m.path, m.kind, size_kb, pct
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Pledge Bundle Analysis</title>
    <style>
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}
        body {{ background: #0a0a0a; color: #e0e0e0; font-family: ui-monospace, monospace; padding: 2rem; }}
        h1 {{ color: #6366f1; margin-bottom: 1rem; }}
        .stats {{ display: flex; gap: 2rem; margin-bottom: 2rem; }}
        .stat {{ background: #1a1a1a; padding: 1rem; border-radius: 8px; border: 1px solid #333; }}
        .stat-label {{ color: #888; font-size: 0.8rem; margin-bottom: 0.25rem; }}
        .stat-value {{ color: #e0e0e0; font-size: 1.5rem; font-weight: 600; }}
        table {{ width: 100%; border-collapse: collapse; margin-top: 1rem; }}
        th {{ text-align: left; padding: 8px 12px; color: #888; border-bottom: 1px solid #333; }}
        td {{ border-bottom: 1px solid #222; }}
        .bar {{ background: #6366f1; height: 4px; border-radius: 2px; }}
    </style>
</head>
<body>
    <h1>Pledge Bundle Analysis</h1>
    <div class="stats">
        <div class="stat">
            <div class="stat-label">Total Size (minified)</div>
            <div class="stat-value">{:.1}KB</div>
        </div>
        <div class="stat">
            <div class="stat-label">Source Size (raw)</div>
            <div class="stat-value">{:.1}KB</div>
        </div>
        <div class="stat">
            <div class="stat-label">Modules</div>
            <div class="stat-value">{}</div>
        </div>
        <div class="stat">
            <div class="stat-label">Chunks</div>
            <div class="stat-value">{}</div>
        </div>
        <div class="stat">
            <div class="stat-label">Duplicates</div>
            <div class="stat-value">{}</div>
        </div>
    </div>
    <h2 style="color:#888;font-size:1rem;margin-bottom:0.5rem;">Largest Modules</h2>
    <table>
        <thead><tr><th>Path</th><th>Type</th><th style="text-align:right;">Minified</th><th style="text-align:right;">%</th></tr></thead>
        <tbody>{}</tbody>
    </table>
</body>
</html>"#,
        total_kb,
        original_kb,
        analysis.total_modules,
        analysis.chunks.len(),
        analysis.duplicates.len(),
        module_rows,
    )
}

fn format_bytes(bytes: usize) -> String {
    crate::format_size(bytes)
}

/// Detect circular dependencies in the module graph (#104)
pub fn detect_circular_deps(analysis: &BundleAnalysis) -> Vec<Vec<String>> {
    let mut cycles: Vec<Vec<String>> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut path: Vec<String> = Vec::new();
    let mut path_set: HashSet<String> = HashSet::new();

    // Build adjacency list
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for m in &analysis.modules {
        adj.insert(m.path.clone(), m.dependencies.clone());
    }

    for m in &analysis.modules {
        if !visited.contains(&m.path) {
            dfs_cycle(
                &m.path,
                &adj,
                &mut visited,
                &mut path,
                &mut path_set,
                &mut cycles,
            );
        }
    }

    cycles
}

fn dfs_cycle(
    node: &str,
    adj: &HashMap<String, Vec<String>>,
    visited: &mut HashSet<String>,
    path: &mut Vec<String>,
    path_set: &mut HashSet<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    if path_set.contains(node) {
        // Found a cycle — extract it
        let cycle_start = path.iter().position(|p| p == node).unwrap_or(0);
        let cycle: Vec<String> = path[cycle_start..].to_vec();
        if !cycles.contains(&cycle) {
            cycles.push(cycle);
        }
        return;
    }

    if visited.contains(node) {
        return;
    }

    visited.insert(node.to_string());
    path.push(node.to_string());
    path_set.insert(node.to_string());

    if let Some(deps) = adj.get(node) {
        for dep in deps {
            dfs_cycle(dep, adj, visited, path, path_set, cycles);
        }
    }

    path.pop();
    path_set.remove(node);
}

/// Generate interactive force-directed dependency graph HTML (#104)
pub fn generate_dependency_graph_html(analysis: &BundleAnalysis) -> String {
    let cycles = detect_circular_deps(analysis);
    let cycle_nodes: HashSet<String> = cycles.iter().flat_map(|c| c.iter().cloned()).collect();

    // Build nodes JSON
    let nodes: String = analysis
        .modules
        .iter()
        .map(|m| {
            let is_circular = cycle_nodes.contains(&m.path);
            let size = (m.transformed_size as f64 / 1024.0).clamp(5.0, 50.0);
            let color = if is_circular {
                "#ef4444"
            } else if m.is_entry {
                "#6366f1"
            } else if m.is_css {
                "#22c55e"
            } else {
                "#888"
            };
            format!(
                r#"{{"id":"{}","label":"{}","size":{},"color":"{}","circular":{}}}"#,
                m.path.replace('"', "\\\""),
                m.path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&m.path)
                    .replace('"', "\\\""),
                size,
                color,
                is_circular,
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    // Build edges JSON
    let edges: String = analysis
        .modules
        .iter()
        .flat_map(|m| {
            m.dependencies
                .iter()
                .filter_map(|dep| {
                    // Only include edges where both endpoints exist
                    if analysis.modules.iter().any(|n| n.path == *dep) {
                        Some(format!(
                            r#"{{"source":"{}","target":"{}"}}"#,
                            m.path.replace('"', "\\\""),
                            dep.replace('"', "\\\""),
                        ))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join(",");

    let cycle_count = cycles.len();
    let cycle_list: String = cycles
        .iter()
        .map(|c| format!("  - {}", c.join(" → ")))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Pledge Dependency Graph</title>
    <style>
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}
        body {{ background: #0a0a0a; color: #e0e0e0; font-family: ui-monospace, monospace; overflow: hidden; }}
        h1 {{ color: #6366f1; padding: 1rem; font-size: 1.2rem; }}
        canvas {{ display: block; }}
        .legend {{ position: fixed; top: 1rem; right: 1rem; background: #1a1a1a; padding: 0.75rem; border-radius: 8px; border: 1px solid #333; font-size: 0.8rem; }}
        .legend-item {{ display: flex; align-items: center; gap: 0.5rem; margin: 0.25rem 0; }}
        .legend-dot {{ width: 12px; height: 12px; border-radius: 50%; }}
        .cycles {{ position: fixed; bottom: 1rem; left: 1rem; background: #1a1a1a; padding: 1rem; border-radius: 8px; border: 1px solid #333; max-width: 400px; max-height: 200px; overflow-y: auto; font-size: 0.75rem; }}
        .cycles h2 {{ color: #ef4444; font-size: 0.9rem; margin-bottom: 0.5rem; }}
    </style>
</head>
<body>
    <h1>Pledge Dependency Graph — {} modules, {} edges, {} circular dep(s)</h1>
    <canvas id="graph"></canvas>
    <div class="legend">
        <div class="legend-item"><div class="legend-dot" style="background:#6366f1;"></div>Entry</div>
        <div class="legend-item"><div class="legend-dot" style="background:#22c55e;"></div>CSS</div>
        <div class="legend-item"><div class="legend-dot" style="background:#888;"></div>Module</div>
        <div class="legend-item"><div class="legend-dot" style="background:#ef4444;"></div>Circular</div>
    </div>
    {}
    <script>
        const nodes = [{}];
        const edges = [{}];
        const canvas = document.getElementById('graph');
        const ctx = canvas.getContext('2d');
        canvas.width = window.innerWidth;
        canvas.height = window.innerHeight;
        // Simple force-directed layout
        const W = canvas.width, H = canvas.height;
        nodes.forEach((n, i) => {{ n.x = Math.random()*W; n.y = Math.random()*H; n.vx = 0; n.vy = 0; }});
        const idMap = {{}}; nodes.forEach((n, i) => idMap[n.id] = i);
        function tick() {{
            // Repulsion
            for (let i = 0; i < nodes.length; i++) {{
                for (let j = i+1; j < nodes.length; j++) {{
                    let dx = nodes[i].x - nodes[j].x, dy = nodes[i].y - nodes[j].y;
                    let d = Math.sqrt(dx*dx+dy*dy)||1;
                    let f = 500/(d*d);
                    nodes[i].vx += dx/d*f; nodes[i].vy += dy/d*f;
                    nodes[j].vx -= dx/d*f; nodes[j].vy -= dy/d*f;
                }}
            }}
            // Attraction (edges)
            edges.forEach(e => {{
                let i = idMap[e.source], j = idMap[e.target];
                if (i===undefined||j===undefined) return;
                let dx = nodes[j].x-nodes[i].x, dy = nodes[j].y-nodes[i].y;
                let d = Math.sqrt(dx*dx+dy*dy)||1;
                let f = (d-150)*0.01;
                nodes[i].vx += dx/d*f; nodes[i].vy += dy/d*f;
                nodes[j].vx -= dx/d*f; nodes[j].vy -= dy/d*f;
            }});
            // Update positions
            nodes.forEach(n => {{ n.x += n.vx*0.1; n.y += n.vy*0.1; n.vx*=0.9; n.vy*=0.9;
                n.x = Math.max(n.size, Math.min(W-n.size, n.x));
                n.y = Math.max(n.size, Math.min(H-n.size, n.y)); }});
            // Draw
            ctx.clearRect(0,0,W,H);
            ctx.strokeStyle = '#333'; ctx.lineWidth = 1;
            edges.forEach(e => {{
                let i = idMap[e.source], j = idMap[e.target];
                if (i===undefined||j===undefined) return;
                ctx.beginPath(); ctx.moveTo(nodes[i].x,nodes[i].y); ctx.lineTo(nodes[j].x,nodes[j].y); ctx.stroke();
            }});
            nodes.forEach(n => {{
                ctx.fillStyle = n.color;
                ctx.beginPath(); ctx.arc(n.x, n.y, n.size, 0, 2*Math.PI); ctx.fill();
                if (n.size > 15) {{ ctx.fillStyle = '#e0e0e0'; ctx.font = '10px monospace'; ctx.fillText(n.label, n.x+n.size+2, n.y); }}
            }});
            requestAnimationFrame(tick);
        }}
        tick();
    </script>
</body>
</html>"#,
        analysis.total_modules,
        edges.matches(',').count() + if edges.is_empty() { 0 } else { 1 },
        cycle_count,
        if cycle_count > 0 {
            format!(
                "<div class=\"cycles\"><h2>Circular Dependencies</h2><pre>{}</pre></div>",
                cycle_list
            )
        } else {
            String::new()
        },
        nodes,
        edges,
    )
}

/// #65: Generate an HTML flamegraph visualization of the bundle.
/// Shows modules as horizontal bars stacked by chunk, with width proportional to size.
/// Supports hover tooltips with module details and click-to-zoom.
pub fn generate_flamegraph_html(analysis: &BundleAnalysis) -> String {
    // Build flamegraph data: group modules by chunk, sort by size descending
    let mut chunk_data: Vec<(String, usize, Vec<&ModuleAnalysis>)> = Vec::new();

    for chunk in &analysis.chunks {
        let chunk_modules: Vec<&ModuleAnalysis> = analysis
            .modules
            .iter()
            .filter(|m| chunk.modules.contains(&m.path))
            .collect();
        chunk_data.push((chunk.name.clone(), chunk.size, chunk_modules));
    }

    // Also include unchunked modules
    let chunked_paths: std::collections::HashSet<&str> = analysis
        .chunks
        .iter()
        .flat_map(|c| c.modules.iter().map(|s| s.as_str()))
        .collect();
    let unchunked: Vec<&ModuleAnalysis> = analysis
        .modules
        .iter()
        .filter(|m| !chunked_paths.contains(m.path.as_str()))
        .collect();
    if !unchunked.is_empty() {
        let total: usize = unchunked.iter().map(|m| m.transformed_size).sum();
        chunk_data.push(("(unchunked)".to_string(), total, unchunked));
    }

    // Sort chunks by size descending
    chunk_data.sort_by_key(|a| std::cmp::Reverse(a.1));

    let max_size = chunk_data.iter().map(|(_, s, _)| *s).max().unwrap_or(1);

    // Generate flamegraph bars as JSON
    let bars_json: Vec<serde_json::Value> = chunk_data
        .iter()
        .map(|(name, size, modules)| {
            let module_list: Vec<serde_json::Value> = modules
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "path": m.path,
                        "size": m.transformed_size,
                        "kind": m.kind,
                        "isEntry": m.is_entry,
                    })
                })
                .collect();
            serde_json::json!({
                "name": name,
                "size": size,
                "width": (*size as f64 / max_size as f64 * 100.0).round(),
                "modules": module_list,
            })
        })
        .collect();

    let bars_str = serde_json::to_string(&bars_json).unwrap_or_default();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<title>PledgePack Flamegraph — Bundle Analyzer</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ background: #1a1a2e; color: #e0e0e0; font-family: 'SF Mono', 'Fira Code', monospace; padding: 20px; }}
  h1 {{ font-size: 18px; margin-bottom: 16px; color: #8be9fd; }}
  .summary {{ display: flex; gap: 20px; margin-bottom: 20px; font-size: 13px; }}
  .summary span {{ color: #bd93f9; }}
  #flamegraph {{ display: flex; flex-direction: column; gap: 2px; }}
  .bar {{ display: flex; height: 28px; border-radius: 3px; overflow: hidden; cursor: pointer; transition: opacity 0.2s; }}
  .bar:hover {{ opacity: 0.85; }}
  .bar-label {{ display: flex; align-items: center; padding: 0 10px; white-space: nowrap; font-size: 12px; color: #fff; }}
  .bar-size {{ margin-left: auto; padding-right: 10px; font-size: 11px; opacity: 0.7; }}
  .module-list {{ margin-left: 20px; border-left: 2px solid #444; padding-left: 12px; margin-top: 4px; display: none; }}
  .bar.expanded + .module-list {{ display: block; }}
  .module-item {{ display: flex; align-items: center; height: 22px; border-radius: 2px; margin: 1px 0; cursor: pointer; transition: opacity 0.2s; }}
  .module-item:hover {{ opacity: 0.8; }}
  .module-item .name {{ padding: 0 8px; font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }}
  .module-item .size {{ margin-left: auto; padding-right: 8px; font-size: 10px; opacity: 0.6; }}
  .tooltip {{ position: fixed; background: #16213e; border: 1px solid #444; padding: 8px 12px; border-radius: 4px; font-size: 12px; pointer-events: none; z-index: 100; display: none; max-width: 400px; }}
  .legend {{ margin-top: 20px; font-size: 12px; color: #888; }}
  .legend span {{ display: inline-block; width: 12px; height: 12px; border-radius: 2px; margin-right: 4px; vertical-align: middle; }}
</style>
</head>
<body>
<h1>🔥 PledgePack Flamegraph</h1>
<div class="summary">
  <div>Total modules: <span>{}</span></div>
  <div>Total size: <span>{}</span></div>
  <div>Chunks: <span>{}</span></div>
</div>
<div id="flamegraph"></div>
<div class="legend">
  <span style="background:#ff5555"></span> Entry
  <span style="background:#50fa7b"></span> CSS
  <span style="background:#f1fa8c"></span> JS/TS
  <span style="background:#bd93f9"></span> Other
</div>
<div class="tooltip" id="tooltip"></div>
<script>
  const bars = {};
  const tooltip = document.getElementById('tooltip');
  const container = document.getElementById('flamegraph');

  function colorFor(kind, isEntry) {{
    if (isEntry) return '#ff5555';
    if (kind === 'css' || kind === 'sass') return '#50fa7b';
    if (kind === 'js' || kind === 'ts' || kind === 'jsx' || kind === 'tsx') return '#f1fa8c';
    return '#bd93f9';
  }}

  function formatSize(bytes) {{
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1048576) return (bytes/1024).toFixed(1) + ' KB';
    return (bytes/1048576).toFixed(1) + ' MB';
  }}

  bars.forEach(bar => {{
    const row = document.createElement('div');
    row.className = 'bar';
    row.style.width = Math.max(bar.width, 5) + '%';
    row.style.background = colorFor('chunk', false);
    row.innerHTML = '<span class="bar-label">' + bar.name + '</span><span class="bar-size">' + formatSize(bar.size) + '</span>';
    row.onclick = () => row.classList.toggle('expanded');

    const modList = document.createElement('div');
    modList.className = 'module-list';
    bar.modules.sort((a,b) => b.size - a.size).forEach(m => {{
      const item = document.createElement('div');
      item.className = 'module-item';
      item.style.width = Math.max((m.size / bar.size * 100), 3) + '%';
      item.style.background = colorFor(m.kind, m.isEntry);
      item.innerHTML = '<span class="name">' + m.path + '</span><span class="size">' + formatSize(m.size) + '</span>';
      item.onmousemove = (e) => {{
        tooltip.style.display = 'block';
        tooltip.style.left = (e.clientX + 12) + 'px';
        tooltip.style.top = (e.clientY + 12) + 'px';
        tooltip.innerHTML = '<strong>' + m.path + '</strong><br>Size: ' + formatSize(m.size) + '<br>Kind: ' + m.kind + (m.isEntry ? '<br><em>Entry point</em>' : '');
      }};
      item.onmouseleave = () => tooltip.style.display = 'none';
      modList.appendChild(item);
    }});
    container.appendChild(row);
    container.appendChild(modList);
  }});
</script>
</body>
</html>"#,
        bars_str,
        analysis.total_modules,
        crate::format_size(analysis.total_transformed_size),
        analysis.chunks.len(),
    )
}

/// Find import chains from entry points to a target module (#82 — pledge why)
/// Returns a list of chains, where each chain is a list of module paths
/// from the entry point to the target module.
pub fn find_import_chains(analysis: &BundleAnalysis, target: &str) -> Vec<Vec<String>> {
    let target_lower = target.to_lowercase();

    // Build adjacency list from module dependencies
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let mut entries: Vec<String> = Vec::new();

    for m in &analysis.modules {
        adj.insert(m.path.clone(), m.dependencies.clone());
        if m.is_entry {
            entries.push(m.path.clone());
        }
    }

    // Find modules matching the target
    let matches: Vec<&str> = analysis
        .modules
        .iter()
        .filter(|m| {
            let p = m.path.to_lowercase();
            p.contains(&target_lower)
        })
        .map(|m| m.path.as_str())
        .collect();

    if matches.is_empty() {
        return Vec::new();
    }

    // BFS from each entry to find paths to any matching module
    let mut chains = Vec::new();

    for entry in &entries {
        for target_mod in &matches {
            if let Some(chain) = bfs_path(&adj, entry, target_mod) {
                chains.push(chain);
            }
        }
    }

    // Deduplicate
    chains.sort();
    chains.dedup();

    chains
}

/// BFS to find a path from source to target in the dependency graph
fn bfs_path(adj: &HashMap<String, Vec<String>>, source: &str, target: &str) -> Option<Vec<String>> {
    if source == target {
        return Some(vec![source.to_string()]);
    }

    let mut visited = HashSet::new();
    let mut queue: Vec<(String, Vec<String>)> =
        vec![(source.to_string(), vec![source.to_string()])];

    while let Some((node, path)) = queue.pop() {
        if visited.contains(&node) {
            continue;
        }
        visited.insert(node.clone());

        if let Some(deps) = adj.get(&node) {
            for dep in deps {
                if dep == target {
                    let mut full_path = path.clone();
                    full_path.push(dep.clone());
                    return Some(full_path);
                }
                if !visited.contains(dep) {
                    let mut new_path = path.clone();
                    new_path.push(dep.clone());
                    queue.push((dep.clone(), new_path));
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BuildMode, Framework, PledgeConfig};

    #[test]
    fn gzip_len_is_smaller_than_repetitive_input_and_zero_safe() {
        let text = "export const a = 1;\n".repeat(200);
        assert!(gzip_len(text.as_bytes()) < text.len() / 4);
        assert!(gzip_len(b"") > 0); // gzip header of an empty stream
    }

    #[test]
    fn summaries_label_raw_minified_and_gzip() {
        let a = BundleAnalysis {
            total_modules: 2,
            total_original_size: 5825,
            total_transformed_size: 3277,
            total_gzip_size: 1500,
            modules: vec![],
            chunks: vec![],
            duplicates: vec![],
            largest_modules: vec![],
        };
        let line = format_size_summary(&a);
        assert!(line.contains("raw") && line.contains("minified") && line.contains("gzip"));
        assert!(line.contains("2 modules"));
    }

    /// `why` and `analyze` share `build_and_analyze`, so the same project
    /// yields identical (minified) numbers whatever mode the config carries.
    #[tokio::test]
    async fn analysis_is_mode_independent_and_minified() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/index.ts"),
            "// a long explanatory comment that a minifier drops\nexport function   compute( firstValue: number,   secondValue: number ) {\n    const   result = firstValue + secondValue;\n    return   result;\n}\nconsole.log( compute( 1, 2 ) );\n",
        )
        .unwrap();
        let mk = |mode| PledgeConfig {
            root: tmp.path().to_path_buf(),
            entry: vec!["src/index.ts".to_string()],
            framework: Framework::Auto,
            mode,
            cache: crate::config::CacheConfig {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let (_e1, dev) = build_and_analyze(&mk(BuildMode::Development)).await.unwrap();
        let (_e2, prod) = build_and_analyze(&mk(BuildMode::Production)).await.unwrap();
        assert_eq!(dev.total_transformed_size, prod.total_transformed_size);
        assert_eq!(dev.total_gzip_size, prod.total_gzip_size);
        assert!(prod.total_transformed_size > 0);
        assert!(
            prod.total_transformed_size < prod.total_original_size,
            "minified output ({}) must be smaller than the raw source ({})",
            prod.total_transformed_size,
            prod.total_original_size
        );
        assert!(prod.total_gzip_size > 0);
    }
}
