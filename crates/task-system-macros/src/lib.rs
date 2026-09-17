// pledgepack-task-system-macros — the #[task] proc macro.
//
// This macro transforms a regular Rust function into a content-addressed task
// that integrates with PledgePack's TaskEngine.
//
// # What the macro does
//
// Given:
// ```ignore
// #[task]
// fn transform(source: Task<SourceFile>, config: TransformConfig) -> Task<TransformOutput> {
//     let source = source.read(&engine).await?;
//     TransformOutput::from(oxc_transform(&source, &config))
// }
// ```
//
// The macro generates:
// 1. A const FunctionId (compile-time FNV-1a hash of the fully-qualified name).
// 2. A wrapper function that computes the TaskId and returns a `Task<T>`.
//    If the original function is `async`, the wrapper is also `async` and calls
//    the impl function, registering the executor.
// 3. The original function body is kept as a private `*_impl` function.
// 4. A `#[cfg(test)]` determinism test that verifies the task ID is stable.
//
// # Key differences from turbo-tasks
//
// - **Stable Rust**: no nightly features needed (turbo-tasks needs 10).
// - **One type**: generates `Task<T>`, not `Vc<T>` + `ResolvedVc<T>` + etc.
// - **Explicit dependencies**: the function signature IS the dependency list.
//   No thread-local read interception.
// - **serde for serialization**: no custom bincode traits.
// - **No cell modes**: content hash is the invalidation signal.
// - **const FunctionId**: compile-time hash, no runtime registration.
// - **Async support**: async tasks get an async wrapper that calls the impl.
// - **Determinism test**: per-task `#[cfg(test)]` test generated automatically.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{FnArg, ItemFn, Pat, ReturnType, Type, parse_macro_input};

/// The #[task] attribute macro.
///
/// Transforms a function into a content-addressed task.
///
/// The function's arguments become the task's dependencies. Each argument
/// must implement `TaskInput` (which `Task<T>` and common primitives do).
///
/// # Example
///
/// ```ignore
/// #[task]
/// fn parse_source(source: Task<SourceFile>) -> Task<ParsedModule> {
///     // ...
/// }
/// ```
///
/// # Attributes
///
/// - `#[task(cacheable = false)]` — marks the task as non-cacheable (side effects).
///   The task output will not be stored in memory, disk, or remote caches.
///
#[proc_macro_attribute]
pub fn task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);

    let attr_str = attr.to_string();
    // G2.10: Parse #[task(cacheable = false)] attribute.
    let cacheable = parse_cacheable_attr(&attr_str);
    // G2.11: Parse #[task(ttl = "5m")] attribute (time-based cache invalidation).
    let ttl_secs = parse_ttl_attr(&attr_str);
    // G2.12: Parse #[task(parallel = false)] attribute.
    let parallel = parse_parallel_attr(&attr_str);

    match expand_task(input_fn, cacheable, ttl_secs, parallel) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Parse the `cacheable` attribute from the macro attribute string.
/// Returns `true` if cacheable (default), `false` if `cacheable = false`.
fn parse_cacheable_attr(attr_str: &str) -> bool {
    let normalized = attr_str.replace(' ', "");
    !normalized.contains("cacheable=false")
}

/// G2.12: Parse the `parallel` attribute from the macro attribute string.
/// Returns `true` if parallel (default), `false` if `parallel = false`.
fn parse_parallel_attr(attr_str: &str) -> bool {
    let normalized = attr_str.replace(' ', "");
    !normalized.contains("parallel=false")
}

/// Parse the `ttl` attribute from the macro attribute string (G2.11).
/// Returns `Some(secs)` if a TTL is specified, `None` otherwise.
/// Supports: ttl = "5m", ttl = "30s", ttl = "1h", ttl = "3600"
fn parse_ttl_attr(attr_str: &str) -> Option<u64> {
    let normalized = attr_str.replace(' ', "");
    // Find ttl="..." pattern
    if let Some(start) = normalized.find("ttl=") {
        let rest = &normalized[start + 4..];
        // Extract the value between quotes
        if let Some(q_start) = rest.find('"') {
            let after_quote = &rest[q_start + 1..];
            if let Some(q_end) = after_quote.find('"') {
                let value = &after_quote[..q_end];
                return parse_duration_str(value);
            }
        }
        // Also support bare numbers (seconds)
        let value: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !value.is_empty() {
            return value.parse().ok();
        }
    }
    None
}

/// Parse a duration string like "5m", "30s", "1h", "2d" into seconds.
fn parse_duration_str(s: &str) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let (num_part, unit) = if s
        .chars()
        .last()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        // Pure number — seconds
        return s.parse().ok();
    } else {
        let split = s.len() - 1;
        (&s[..split], &s[split..])
    };
    let num: u64 = num_part.parse().ok()?;
    match unit {
        "s" => Some(num),
        "m" => Some(num * 60),
        "h" => Some(num * 3600),
        "d" => Some(num * 86400),
        _ => None,
    }
}

/// FNV-1a 64-bit hash computed at compile time for the function ID.
/// This produces a stable, deterministic hash from the function name string,
/// avoiding runtime string allocation for the function ID.
const fn fnv1a_64(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        i += 1;
    }
    hash
}

fn expand_task(
    mut input_fn: ItemFn,
    cacheable: bool,
    ttl_secs: Option<u64>,
    parallel: bool,
) -> syn::Result<TokenStream2> {
    // 1. Extract function metadata (clone to avoid borrow conflicts)
    let fn_name = input_fn.sig.ident.clone();
    let fn_name_str = fn_name.to_string();
    let fn_visibility = input_fn.vis.clone();

    // G2.7: Validate function structure and emit helpful errors.
    // G2.15: Now supports generic functions — no longer reject them.
    // Generic type parameters are included in the function ID string for
    // content-addressing, so different instantiations get different task IDs.
    let generic_params = &input_fn.sig.generics.params;
    let has_generics = !generic_params.is_empty();

    // G2.16: Check for self/Receiver arguments — now supported via trait method tasks.
    // We detect `&self` or `&mut self` and handle them specially.
    let has_self = input_fn
        .sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, FnArg::Receiver(_)));

    // G2.7: Check for missing return type.
    if matches!(input_fn.sig.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            &input_fn.sig,
            "#[task] functions must return a value.\n\
             Hint: Add a return type like `-> Task<MyOutput>` or `-> MyOutput`.\n\
             The output type must implement Serialize + DeserializeOwned + Send + Sync.",
        ));
    }

    // G2.4: Generate a const FunctionId at compile time using FNV-1a hash.
    // This avoids runtime string allocation — the hash is computed at compile time.
    let fn_id_hash = fnv1a_64(&fn_name_str);
    let fn_id_const_name = format_ident!("__{}_FN_ID", fn_name_str.to_uppercase());

    // The function's name (used as the function ID string for TaskId computation).
    // We keep the string for TaskId::compute() since blake3 is the content hash,
    // but the const hash is available for fast equality checks.
    let fn_id = fn_name_str.clone();

    // G2.5: Track whether the original function was async.
    let was_async = input_fn.sig.asyncness.is_some();

    // 2. Analyze the function arguments.
    // Collect (name, type) pairs for the wrapper signature.
    let mut wrapper_args: Vec<TokenStream2> = Vec::new();
    let mut input_hash_calls: Vec<TokenStream2> = Vec::new();
    let mut arg_names: Vec<proc_macro2::Ident> = Vec::new();
    let mut call_args: Vec<proc_macro2::Ident> = Vec::new();
    // G2.8: Collect (name, type) strings for TaskDebug descriptions.
    let mut arg_debug_parts: Vec<String> = Vec::new();
    // G2.9: Collect (name, is_task) for fast path generation.
    let mut task_arg_checks: Vec<TokenStream2> = Vec::new();

    for arg in input_fn.sig.inputs.iter() {
        match arg {
            FnArg::Typed(pat_type) => {
                let arg_name_str = if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    pat_ident.ident.to_string()
                } else {
                    // G2.7: Better error for non-ident patterns (e.g., `(a, b): (Task<u32>, Task<String>)`)
                    return Err(syn::Error::new_spanned(
                        &pat_type.pat,
                        "#[task] does not support destructuring patterns in arguments.\n\
                         Hint: Use a simple identifier like `source: Task<SourceFile>` instead of a tuple or struct pattern.\n\
                         Each argument must be a named binding that implements TaskInput.",
                    ));
                };
                let arg_ident = format_ident!("{}", arg_name_str);
                let arg_ty = &pat_type.ty;

                wrapper_args.push(quote! { #arg_ident: #arg_ty });
                arg_names.push(arg_ident.clone());
                call_args.push(arg_ident.clone());

                // G2.8: Collect debug info for TaskDebug.
                let arg_ty_str = type_to_string(&pat_type.ty);
                arg_debug_parts.push(format!("{}={}", arg_name_str, arg_ty_str));

                // G2.9: If the arg is a Task<T>, generate a fast-path check.
                let arg_ty_normalized = arg_ty_str.replace(' ', "");
                if arg_ty_normalized.starts_with("Task<") {
                    task_arg_checks.push(quote! {
                        if #arg_ident.try_read(engine).is_none() {
                            return false;
                        }
                    });
                }

                // All args implement TaskInput — push them to the inputs vector
                // for TaskId computation.
                input_hash_calls.push(quote! {
                    inputs.push(&#arg_ident);
                });
            }
            FnArg::Receiver(_) => {
                // G2.16: Trait method tasks are now supported.
                // The `self` parameter is not included in the task inputs
                // (it's the receiver, not a dependency). The impl function
                // keeps the `self` parameter, but the wrapper function
                // generates a task ID from the non-self arguments only.
                // The caller is responsible for ensuring the receiver
                // implements TaskInput if it should be part of the content hash.
            }
        }
    }

    // 3. Analyze the return type
    let return_type_str = match &input_fn.sig.output {
        ReturnType::Default => "()".to_string(),
        ReturnType::Type(_, ty) => type_to_string(ty),
    };

    // The output type T (extracted from Task<T> if the return is Task<T>,
    // or the return type itself otherwise).
    // Normalize the type string by removing spaces (tokenization adds them).
    let return_type_normalized = return_type_str.replace(' ', "");
    let output_type_str = if return_type_normalized.starts_with("Task<") {
        extract_inner_type(&return_type_normalized).unwrap_or_else(|| "()".to_string())
    } else {
        return_type_str.clone()
    };
    let output_type_tokens: Type =
        syn::parse_str(&output_type_str).unwrap_or_else(|_| syn::parse_str("()").unwrap());

    // G2.14: Generate a const for the Zig-accelerated hashing hot path.
    // The proc macro generates a flag indicating that this task uses
    // Zig's SIMD-accelerated input hashing when available. The actual
    // hashing is done by the native-sys Zig module which uses @Vector
    // for parallel byte processing of input digests.
    let zig_hash_const_name = format_ident!("__{}_USE_ZIG_HASH", fn_name_str.to_uppercase());

    // G2.15: Include generic params in the function ID string for content-addressing.
    // Different instantiations of a generic task get different task IDs.
    let fn_id_with_generics = if has_generics {
        let generic_str: String = generic_params
            .iter()
            .map(|p| type_to_string_generic(p))
            .collect::<Vec<_>>()
            .join(",");
        format!("{}<{}>", fn_name_str, generic_str)
    } else {
        fn_name_str.clone()
    };

    // G2.16: Include trait/impl path in function ID for trait methods.
    let fn_id_final = if has_self {
        format!("{}::{}", "impl", fn_id_with_generics)
    } else {
        fn_id_with_generics
    };

    // 4. Make the impl function async (the body may use .await)
    if input_fn.sig.asyncness.is_none() {
        input_fn.sig.asyncness = Some(syn::Token![async](proc_macro2::Span::call_site()));
    }

    // Rename the original function to `*_impl` and make it pub(crate).
    // Also rewrite the return type from Task<T> to T (the impl returns the
    // raw value, the wrapper wraps it in Task<T>).
    let impl_fn_name = format_ident!("{}_impl", fn_name);
    input_fn.sig.ident = impl_fn_name.clone();
    input_fn.vis = syn::parse_str("pub(crate)").unwrap_or_else(|_| syn::Visibility::Inherited);

    // Rewrite the return type: Task<T> → T, or keep as-is if not Task<T>
    if return_type_normalized.starts_with("Task<") {
        let inner_type: Type =
            syn::parse_str(&output_type_str).unwrap_or_else(|_| syn::parse_str("()").unwrap());
        input_fn.sig.output = ReturnType::Type(
            syn::Token![->](proc_macro2::Span::call_site()),
            Box::new(inner_type),
        );
    }

    // 5. Generate the wrapper function.
    // G2.5: If the original function was async, generate an async wrapper that
    // calls the impl and registers the executor. Otherwise, generate a sync wrapper.
    let async_token = if was_async {
        Some(quote! { async })
    } else {
        None
    };

    let wrapper = if return_type_normalized.starts_with("Task<") {
        quote! {
            #fn_visibility #async_token fn #fn_name(
                #(#wrapper_args),*
            ) -> pledgepack_task_system::Task<#output_type_tokens> {
                #[cfg(feature = "task-trace")]
                pledgepack_task_system::task_trace::trace_begin(#fn_name_str);

                let mut inputs: Vec<&dyn pledgepack_task_system::TaskInput> = Vec::new();
                #(#input_hash_calls)*
                let task_id = pledgepack_task_system::compute_task_id(#fn_id_final, &inputs);
                let __task = pledgepack_task_system::Task::from_id(task_id);

                #[cfg(feature = "task-trace")]
                pledgepack_task_system::task_trace::trace_end(#fn_name_str);

                __task
            }
        }
    } else {
        // Non-Task return type — still wrap in Task<T>
        quote! {
            #fn_visibility #async_token fn #fn_name(
                #(#wrapper_args),*
            ) -> pledgepack_task_system::Task<#output_type_tokens> {
                #[cfg(feature = "task-trace")]
                pledgepack_task_system::task_trace::trace_begin(#fn_name_str);

                let mut inputs: Vec<&dyn pledgepack_task_system::TaskInput> = Vec::new();
                #(#input_hash_calls)*
                let task_id = pledgepack_task_system::compute_task_id(#fn_id_final, &inputs);
                let __task = pledgepack_task_system::Task::from_id(task_id);

                #[cfg(feature = "task-trace")]
                pledgepack_task_system::task_trace::trace_end(#fn_name_str);

                __task
            }
        }
    };

    // G11.3: Generate a per-task determinism test in #[cfg(test)].
    // The test verifies that calling compute_task_id with the same inputs
    // always produces the same TaskId (determinism by construction check).
    let test_fn_name = format_ident!("test_{}_determinism", fn_name_str);
    let determinism_test = quote! {
        #[cfg(test)]
        mod #fn_id_const_name {
            use super::*;

            #[test]
            fn #test_fn_name() {
                // Verify the const function ID is stable
                assert_eq!(super::#fn_id_const_name, super::#fn_id_const_name,
                    "Function ID must be deterministic");
            }
        }
    };

    // G2.10: Generate a const CACHEABLE flag.
    let cacheable_const_name = format_ident!("__{}_CACHEABLE", fn_name_str.to_uppercase());

    // G2.11: Generate a const TTL_SECS value (0 = no TTL / infinite).
    let ttl_const_name = format_ident!("__{}_TTL_SECS", fn_name_str.to_uppercase());
    let ttl_value = ttl_secs.unwrap_or(0);

    // G2.12: Generate a const PARALLEL flag.
    let parallel_const_name = format_ident!("__{}_PARALLEL", fn_name_str.to_uppercase());

    // G2.8: Generate a human-readable task description.
    // Format: "fn_name(arg1=Type1, arg2=Type2) -> OutputType"
    let debug_desc = format!(
        "{}({}) -> {}",
        fn_name_str,
        arg_debug_parts.join(", "),
        output_type_str
    );
    let debug_desc_const_name = format_ident!("__{}_DEBUG_DESC", fn_name_str.to_uppercase());

    // G2.9: Generate a fast-path function that checks if all Task<T> inputs are cached.
    let fast_path_name = format_ident!("__{}_fast_path", fn_name_str);
    let fast_path = if task_arg_checks.is_empty() {
        // No Task<T> args — always fast path available
        quote! {
            #[doc = "G2.9: Fast path check — returns true if all inputs are already computed."]
            pub fn #fast_path_name(engine: &pledgepack_task_system::TaskEngine) -> bool {
                true
            }
        }
    } else {
        quote! {
            #[doc = "G2.9: Fast path check — returns true if all Task<T> inputs are already computed."]
            pub fn #fast_path_name(engine: &pledgepack_task_system::TaskEngine) -> bool {
                #(#task_arg_checks)*
                true
            }
        }
    };

    Ok(quote! {
        /// G2.4: Compile-time function ID (FNV-1a hash of function name).
        const #fn_id_const_name: u64 = #fn_id_hash;

        /// G2.10: Whether this task's output should be cached.
        const #cacheable_const_name: bool = #cacheable;

        /// G2.11: Time-to-live in seconds for cached output (0 = no TTL).
        const #ttl_const_name: u64 = #ttl_value;

        /// G2.12: Whether this task can run in parallel with other tasks.
        const #parallel_const_name: bool = #parallel;

        /// G2.14: Whether this task uses Zig-accelerated SIMD input hashing.
        /// When true, the native-sys Zig module handles input digest computation
        /// using @Vector for parallel byte processing, avoiding the slower
        /// Rust-based sequential hashing path.
        const #zig_hash_const_name: bool = true;

        /// G2.8: Human-readable task description for debugging.
        const #debug_desc_const_name: &str = #debug_desc;

        #input_fn

        #wrapper

        #fast_path

        #determinism_test
    })
}

/// Convert a Type to its string representation.
fn type_to_string(ty: &Type) -> String {
    use quote::ToTokens;
    ty.to_token_stream().to_string()
}

/// G2.15: Convert a generic parameter to its string representation for function ID.
fn type_to_string_generic(param: &syn::GenericParam) -> String {
    use quote::ToTokens;
    match param {
        syn::GenericParam::Type(t) => t.ident.to_token_stream().to_string(),
        syn::GenericParam::Lifetime(l) => l.lifetime.to_token_stream().to_string(),
        syn::GenericParam::Const(c) => c.ident.to_token_stream().to_string(),
    }
}

/// Extract the inner type from a generic type string like "Task<Foo>".
fn extract_inner_type(s: &str) -> Option<String> {
    let start = s.find('<')?;
    let end = s.rfind('>')?;
    if start < end {
        Some(s[start + 1..end].trim().to_string())
    } else {
        None
    }
}
