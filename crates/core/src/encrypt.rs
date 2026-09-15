// Build-time string obfuscation (#109)
//
// Replaces configured string literals in source with an XOR-obfuscated,
// base64-encoded form at build time, decoded at runtime via an injected JS
// shim.
//
// # This is obfuscation, not encryption — by construction, not by omission
//
// PRODUCTION-READINESS-100.md goal 14 asked whether this should use
// `aes-gcm`/`chacha20poly1305` instead of XOR. It shouldn't, and swapping the
// cipher wouldn't fix the actual problem: the decryption key is embedded
// directly in the same JS bundle as the "encrypted" strings (see the
// `__pledge_key` constant `encrypt_strings` injects below), because the
// browser has to be able to decode the value at runtime with nothing else
// available. Any client-side scheme — XOR, AES-GCM, ChaCha20 — is reversible
// by anyone who can run the same JS the browser runs, i.e. everyone. A
// stronger cipher raises the bar from "grep the bundle" to "run the
// deobfuscation function with the key that's right there," which is a real
// (if modest) improvement against casual inspection, but it is not
// confidentiality against a motivated reader and must never be used to ship
// anything that needs to actually stay secret (API keys, credentials, etc.
// belong server-side, not in a client bundle, obfuscated or not).
//
// Given that ceiling, this deliberately stays a lightweight, dependency-free,
// fully synchronous transform (real AEAD ciphers decrypt asynchronously via
// the browser's Web Crypto API, which would force every call site of a
// replaced string literal to become `await`-able — a bigger, riskier change
// for a feature that can't deliver real security either way) rather than a
// hand-rolled from-scratch cipher implementation duplicated across Rust and
// JS, which would trade a known, clearly-labeled weakness for the much worse
// risk of a subtly-wrong custom crypto implementation.
use crate::config::EncryptConfig;
use tracing::info;

fn xor_encrypt(data: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        // No encryption with an empty key — return data unchanged
        // (also avoids division-by-zero panic on `key.len()`)
        return data.to_vec();
    }
    data.iter()
        .enumerate()
        .map(|(i, &b)| b ^ key[i % key.len()])
        .collect()
}

/// Generate a 32-byte encryption key from config or randomly
fn get_or_create_key(config: &EncryptConfig) -> Vec<u8> {
    if let Some(ref key_hex) = config.key {
        // Parse hex string to bytes
        if key_hex.len() == 64 {
            (0..key_hex.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(&key_hex[i..i + 2], 16).ok())
                .collect()
        } else {
            // Use the string bytes directly, padded/truncated to 32
            let bytes = key_hex.as_bytes();
            let mut key = vec![0u8; 32];
            for (i, &b) in bytes.iter().enumerate().take(32) {
                key[i] = b;
            }
            key
        }
    } else {
        // Generate a fresh random key for this build. This does NOT need to
        // be reproducible across builds or derivable from anything: the key
        // is embedded directly in the emitted bundle alongside the
        // obfuscated values (see `encrypt_strings`'s `__pledge_key`
        // constant) and is only ever used to encrypt/decrypt within that
        // single build's own output. It previously derived from
        // `SystemTime::now()` (build timestamp) plus the configured key
        // names joined by commas — both guessable/reconstructable by anyone
        // who can see roughly when the build ran (e.g. from an HTTP
        // `Last-Modified` header or a git commit timestamp) and the
        // (non-secret) list of env var names being obfuscated. See
        // PRODUCTION-READINESS-100.md goal 15.
        let mut key = vec![0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut key);
        key
    }
}

/// Obfuscate a single string value. See the module-level doc comment above —
/// this is not encryption.
pub fn encrypt_value(value: &str, key: &[u8]) -> String {
    let encrypted = xor_encrypt(value.as_bytes(), key);
    // Base64 encode for safe embedding
    base64_encode(&encrypted)
}

/// Reverse [`encrypt_value`] (used in the runtime shim).
pub fn decrypt_value(encrypted: &str, key: &[u8]) -> String {
    let decoded = base64_decode(encrypted);
    let decrypted = xor_encrypt(&decoded, key);
    String::from_utf8_lossy(&decrypted).to_string()
}

/// Transform source code: encrypt sensitive string literals
/// Replaces string literals matching configured keys with encrypted versions
pub fn encrypt_strings(code: &str, config: &EncryptConfig) -> anyhow::Result<(String, Vec<u8>)> {
    if !config.enabled || config.keys.is_empty() {
        return Ok((code.to_string(), Vec::new()));
    }

    let key = get_or_create_key(config);
    let mut result = code.to_string();

    // For each configured key, find its value in process.env or define
    // and replace occurrences in the code with encrypted versions
    for key_name in &config.keys {
        // Look up the value from process.env
        if let Ok(value) = std::env::var(key_name)
            && result.contains(&value)
        {
            let encrypted = encrypt_value(&value, &key);
            // Replace the plain-text value with a decryption call
            let replacement = format!("__pledge_decrypt(\"{}\")", encrypted,);
            result = result.replace(&value, &replacement);
        }
    }

    // Inject the decryption shim at the top of the code
    let key_b64 = base64_encode(&key);
    let shim = format!(
        r#"// Build-time string encryption shim (#109)
const __pledge_key = __pledge_b64dec("{}");
function __pledge_b64dec(s) {{
  const chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  let bytes = [];
  for (let i = 0; i < s.length; i += 4) {{
    let n = (chars.indexOf(s[i]) << 18) | (chars.indexOf(s[i+1]) << 12);
    if (s[i+2] !== '=') n |= (chars.indexOf(s[i+2]) << 6);
    if (s[i+3] !== '=') n |= chars.indexOf(s[i+3]);
    bytes.push((n >> 16) & 0xff);
    if (s[i+2] !== '=') bytes.push((n >> 8) & 0xff);
    if (s[i+3] !== '=') bytes.push(n & 0xff);
  }}
  return new Uint8Array(bytes);
}}
function __pledge_decrypt(enc) {{
  const decoded = __pledge_b64dec(enc);
  const result = new Uint8Array(decoded.length);
  for (let i = 0; i < decoded.length; i++) {{
    result[i] = decoded[i] ^ __pledge_key[i % __pledge_key.length];
  }}
  return new TextDecoder().decode(result);
}}

"#,
        key_b64,
    );

    result = format!("{}\n{}", shim, result);

    info!("String encryption: {} keys encrypted", config.keys.len());
    Ok((result, key))
}

/// Simple base64 encoder
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();

    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };

        let n = (b0 << 16) | (b1 << 8) | b2;

        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);

        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        } else {
            result.push('=');
        }
    }

    result
}

/// Simple base64 decoder
fn base64_decode(s: &str) -> Vec<u8> {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = Vec::new();
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let s = s.as_bytes();

    for chunk in s.chunks(4) {
        let mut n = 0u32;
        let mut pad = 0;

        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
            } else {
                let idx = CHARS.iter().position(|&x| x == c).unwrap_or(0);
                n |= (idx as u32) << (18 - i * 6);
            }
        }

        result.push(((n >> 16) & 0xff) as u8);
        if pad < 2 {
            result.push(((n >> 8) & 0xff) as u8);
        }
        if pad < 1 {
            result.push((n & 0xff) as u8);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_round_trip() {
        let key = b"01234567890123456789012345678901".to_vec();
        let value = "super-secret-ish-value";
        let encrypted = encrypt_value(value, &key);
        assert_ne!(encrypted, value);
        assert_eq!(decrypt_value(&encrypted, &key), value);
    }

    #[test]
    fn auto_generated_keys_are_random_not_derived_from_timestamp() {
        // Regression test for goal 15: two calls with no explicit key and
        // identical `keys` lists must NOT produce the same key — the old
        // implementation derived deterministically from
        // `SystemTime::now()` + `keys.join(",")`, so two calls made within
        // the same second (as these two will be) previously collided.
        let config = EncryptConfig {
            enabled: true,
            keys: vec!["API_TOKEN".to_string()],
            key: None,
        };
        let key_a = get_or_create_key(&config);
        let key_b = get_or_create_key(&config);
        assert_eq!(key_a.len(), 32);
        assert_ne!(
            key_a, key_b,
            "auto-generated keys must be random, not derived from guessable build-time state"
        );
    }

    #[test]
    fn explicit_hex_key_is_used_verbatim() {
        let hex_key = "0".repeat(64); // 32 zero bytes, hex-encoded
        let config = EncryptConfig {
            enabled: true,
            keys: vec![],
            key: Some(hex_key),
        };
        let key = get_or_create_key(&config);
        assert_eq!(key, vec![0u8; 32]);
    }

    #[test]
    fn base64_round_trip() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let encoded = base64_encode(data);
        let decoded = base64_decode(&encoded);
        assert_eq!(decoded, data);
    }
}
