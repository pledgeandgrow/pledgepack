// Test WASM Plugin Guest for PledgePack
//
// This is a minimal plugin that implements the pledgepack-plugin WIT world.
// It demonstrates the vertical slice: a WASM plugin running sandboxed,
// its transform output cached in the task graph.
//
// The plugin does a simple transform: it appends a comment to JS files
// and resolves "virtual:test-plugin" to a virtual module.

wit_bindgen::generate!({
    world: "pledgepack-plugin",
    path: "wit",
});

struct TestPlugin;

impl Guest for TestPlugin {
    fn plugin_metadata() -> PluginMetadata {
        PluginMetadata {
            name: "test-plugin".to_string(),
            version: "0.1.1".to_string(),
            hooks: HookFlags {
                resolve_id: true,
                load: true,
                transform: true,
                transform_index_html: false,
                build_start: false,
                build_end: false,
                generate_bundle: false,
                configure_server: false,
                render_chunk: false,
                handle_hot_update: false,
            },
            apply: Some("all".to_string()),
            enforce: None, // default = "post"
        }
    }

    fn resolve_id(input: ResolveIdInput) -> Option<ResolveIdOutput> {
        // Resolve virtual:test-plugin to a virtual module ID
        if input.source == "virtual:test-plugin" {
            Some(ResolveIdOutput {
                id: "\0virtual:test-plugin".to_string(),
                external: false,
                cache_key: blake3_hash(input.source.as_bytes()),
                schema_version: Some(1),
            })
        } else {
            None
        }
    }

    fn load(input: LoadInput) -> Option<LoadOutput> {
        // Load the virtual module
        if input.id == "\0virtual:test-plugin" {
            Some(LoadOutput {
                code: "export const hello = 'from test-plugin';".to_string(),
                source_map: None,
                cache_key: blake3_hash(input.id.as_bytes()),
                schema_version: Some(1),
            })
        } else {
            None
        }
    }

    fn transform(input: TransformInput) -> Option<TransformOutput> {
        // Append a comment to JS files
        if input.id.ends_with(".js") || input.id.ends_with(".ts") || input.id.ends_with(".jsx") {
            let transformed = format!("// transformed by test-plugin\n{}", input.code);
            let mut hasher_input = Vec::new();
            hasher_input.extend_from_slice(input.code.as_bytes());
            hasher_input.extend_from_slice(input.id.as_bytes());
            Some(TransformOutput {
                code: transformed,
                source_map: None,
                cache_key: blake3_hash(&hasher_input),
                schema_version: Some(1),
            })
        } else {
            None
        }
    }

    fn transform_index_html(_input: HtmlInput) -> Option<HtmlOutput> {
        None
    }

    fn build_start() {}

    fn build_end() {}

    fn generate_bundle() {}

    fn configure_server() -> Option<ServerMiddleware> {
        None
    }

    fn render_chunk(_input: RenderChunkInput) -> Option<RenderChunkOutput> {
        // Not implemented by this test plugin (hooks.render_chunk = false).
        None
    }

    fn handle_hot_update(_input: HotUpdateInput) -> Option<HotUpdateOutput> {
        // Not implemented by this test plugin (hooks.handle_hot_update = false).
        None
    }
}

/// Simple blake3 hash implementation for cache keys.
/// In a real plugin, you'd use the `blake3` crate, but for this test
/// we use a simple FNV-1a hash to avoid extra dependencies.
fn blake3_hash(data: &[u8]) -> String {
    // FNV-1a 64-bit hash (simplified for test plugin)
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", hash)
}

export!(TestPlugin);
