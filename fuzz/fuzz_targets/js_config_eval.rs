#![no_main]

//! `eval_config_module` statically evaluates arbitrary JS/TS config source.
//! Fuzz contract: never panics, always terminates — malformed input returns
//! Err or a partially-evaluated Value, never a crash.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let src = String::from_utf8_lossy(data);
    // Alternate file names so .ts/.js/.mjs/.cjs parser modes all get hit.
    let name = if data.first().is_some_and(|b| b & 1 == 0) {
        "pledge.config.ts"
    } else {
        "pledge.config.mjs"
    };
    let _ = pledgepack_core::js_config::eval_config_module(&src, name);
});
