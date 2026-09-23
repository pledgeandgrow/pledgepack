// Node.js polyfills for browser builds
//
// When `node_polyfills` is enabled in config, bare imports of Node.js
// built-in modules (e.g., "path", "crypto", "buffer", "stream") are
// replaced with browser-compatible polyfills.
//
// This mirrors esbuild's `--inject` and Vite's `define`/`optimizeDeps` behavior.

use std::collections::HashMap;

/// Map of Node.js built-in module names to their browser polyfill source code.
/// These are minimal ESM-compatible polyfills that provide the most commonly used APIs.
pub fn node_polyfills() -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    map.insert("buffer", BUFFER_POLYFILL);
    map.insert("process", PROCESS_POLYFILL);
    map.insert("path", PATH_POLYFILL);
    map.insert("crypto", CRYPTO_POLYFILL);
    map.insert("stream", STREAM_POLYFILL);
    map.insert("util", UTIL_POLYFILL);
    map.insert("events", EVENTS_POLYFILL);
    map.insert("url", URL_POLYFILL);
    map.insert("os", OS_POLYFILL);
    map.insert("fs", FS_POLYFILL);
    map.insert("http", HTTP_POLYFILL);
    map.insert("https", HTTPS_POLYFILL);
    map.insert("net", NET_POLYFILL);
    map.insert("tls", TLS_POLYFILL);
    map.insert("zlib", ZLIB_POLYFILL);
    map.insert("querystring", QUERYSTRING_POLYFILL);
    map.insert("string_decoder", STRING_DECODER_POLYFILL);
    map.insert("timers", TIMERS_POLYFILL);
    map.insert("assert", ASSERT_POLYFILL);
    map.insert("child_process", CHILD_PROCESS_POLYFILL);
    map
}

/// Check if a specifier is a Node.js built-in module.
/// Every `node:…` specifier is a builtin (the prefix is reserved), and bare
/// builtins match on their first path segment so `fs/promises`,
/// `stream/web`, `timers/promises`, … are covered too.
pub fn is_node_builtin(specifier: &str) -> bool {
    if let Some(rest) = specifier.strip_prefix("node:") {
        return !rest.is_empty();
    }
    let head = specifier.split('/').next().unwrap_or(specifier);
    matches!(
        head,
        "buffer"
            | "process"
            | "path"
            | "crypto"
            | "stream"
            | "util"
            | "events"
            | "url"
            | "os"
            | "fs"
            | "http"
            | "https"
            | "net"
            | "tls"
            | "zlib"
            | "querystring"
            | "string_decoder"
            | "timers"
            | "assert"
            | "child_process"
            | "node:path"
            | "node:crypto"
            | "node:buffer"
            | "node:stream"
            | "node:util"
            | "node:events"
            | "node:url"
            | "node:os"
            | "node:fs"
            | "node:http"
            | "node:https"
            | "node:net"
            | "node:tls"
            | "node:zlib"
            | "node:querystring"
            | "node:string_decoder"
            | "node:timers"
            | "node:assert"
            | "node:child_process"
            | "node:process"
    )
}

/// Normalize a node: prefix specifier (e.g., "node:path" → "path")
pub fn normalize_specifier(specifier: &str) -> &str {
    specifier.strip_prefix("node:").unwrap_or(specifier)
}

/// Get the polyfill source for a Node.js built-in module
pub fn get_polyfill(specifier: &str) -> Option<&'static str> {
    let normalized = normalize_specifier(specifier);
    node_polyfills().get(normalized).copied()
}

// ─── Polyfill Source Code ────────────────────────────────────────────

const BUFFER_POLYFILL: &str = r#"// Buffer polyfill
const _buf = new Uint8Array(0);
export const Buffer = {
  from(data, encoding) {
    if (typeof data === 'string') {
      const enc = encoding || 'utf8';
      return new TextEncoder().encode(data);
    }
    return new Uint8Array(data);
  },
  alloc(size, fill) {
    const buf = new Uint8Array(size);
    if (fill !== undefined) buf.fill(fill);
    return buf;
  },
  allocUnsafe(size) { return new Uint8Array(size); },
  concat(lists) {
    let total = 0;
    for (const l of lists) total += l.length;
    const result = new Uint8Array(total);
    let offset = 0;
    for (const l of lists) { result.set(l, offset); offset += l.length; }
    return result;
  },
  isBuffer(x) { return x instanceof Uint8Array; },
};
export default Buffer;
"#;

const PROCESS_POLYFILL: &str = r#"// process polyfill
const _env = {};
try { _env = import.meta.env || {}; } catch(e) {}
export const process = {
  env: _env,
  argv: [],
  cwd: () => '/',
  platform: 'browser',
  version: 'v18.0.0',
  nextTick: (fn, ...args) => queueMicrotask(() => fn(...args)),
  stdout: { write: () => true },
  stderr: { write: () => true },
  on: () => {},
  off: () => {},
  exit: (code) => {},
};
export default process;
"#;

const PATH_POLYFILL: &str = r#"// path polyfill (minimal)
function normalize(p) {
  const parts = p.split('/').filter(Boolean);
  const result = [];
  for (const part of parts) {
    if (part === '..') result.pop();
    else if (part !== '.') result.push(part);
  }
  return (p.startsWith('/') ? '/' : '') + result.join('/');
}
export const path = {
  join: (...args) => normalize(args.join('/')),
  resolve: (...args) => normalize(args.join('/')),
  normalize,
  dirname: (p) => { const i = p.lastIndexOf('/'); return i >= 0 ? p.slice(0, i) : '.'; },
  basename: (p, ext) => { const b = p.split('/').pop() || ''; return ext && b.endsWith(ext) ? b.slice(0, -ext.length) : b; },
  extname: (p) => { const i = p.lastIndexOf('.'); return i >= 0 ? p.slice(i) : ''; },
  sep: '/',
  delimiter: ':',
  relative: (from, to) => normalize(to),
  isAbsolute: (p) => p.startsWith('/'),
};
export default path;
"#;

const CRYPTO_POLYFILL: &str = r#"// crypto polyfill using Web Crypto API
export const crypto = globalThis.crypto || {};
export const webcrypto = globalThis.crypto;
export default crypto;
export function getRandomValues(arr) { return globalThis.crypto.getRandomValues(arr); }
export function randomUUID() { return globalThis.crypto.randomUUID(); }
export function createHash(alg) {
  return {
    update: function(data) { return this; },
    digest: function(enc) {
      // Use SubtleCrypto for real hashing in async context
      return new Uint8Array(32);
    },
  };
}
"#;

const STREAM_POLYFILL: &str = r#"// stream polyfill (minimal)
export class Readable {
  constructor(opts) { this._opts = opts || {}; }
  pipe(dest) { return dest; }
  on() { return this; }
  destroy() {}
}
export class Writable {
  constructor(opts) { this._opts = opts || {}; }
  write() { return true; }
  end() {}
  on() { return this; }
}
export class Transform {
  constructor(opts) { this._opts = opts || {}; }
  pipe(dest) { return dest; }
  on() { return this; }
}
export class Duplex extends Readable {
  constructor(opts) { super(opts); }
  write() { return true; }
  end() {}
}
export const pipeline = (...streams) => streams[streams.length - 1];
export default { Readable, Writable, Transform, Duplex };
"#;

const UTIL_POLYFILL: &str = r#"// util polyfill
export function inspect(obj) { return String(obj); }
export function format(...args) { return args.join(' '); }
export function inherits(ctor, superCtor) { ctor.prototype = Object.create(superCtor.prototype); }
export function promisify(fn) {
  return function(...args) {
    return new Promise((resolve, reject) => {
      fn.call(this, ...args, (err, ...result) => {
        if (err) reject(err); else resolve(result[0]);
      });
    });
  };
}
export function callbackify(fn) {
  return function(...args) {
    const cb = args.pop();
    Promise.resolve(fn.apply(this, args)).then(r => cb(null, r), cb);
  };
}
export function deprecate(fn) { return fn; }
export function isDeepStrictEqual(a, b) { return JSON.stringify(a) === JSON.stringify(b); }
export default { inspect, format, inherits, promisify, callbackify, deprecate, isDeepStrictEqual };
"#;

const EVENTS_POLYFILL: &str = r#"// events polyfill
export class EventEmitter {
  constructor() { this._events = {}; }
  on(event, fn) { (this._events[event] ||= []).push(fn); return this; }
  once(event, fn) { const w = (...a) => { this.off(event, w); fn(...a); }; return this.on(event, w); }
  off(event, fn) { const l = this._events[event]; if (l) this._events[event] = l.filter(f => f !== fn); return this; }
  emit(event, ...args) { const l = this._events[event]; if (l) for (const f of l) f(...args); return true; }
  removeListener() { return this.off(...arguments); }
  removeAllListeners(event) { if (event) delete this._events[event]; else this._events = {}; return this; }
  listenerCount(event) { return (this._events[event] || []).length; }
}
export default EventEmitter;
"#;

const URL_POLYFILL: &str = r#"// url polyfill using built-in URL
export const URL = globalThis.URL;
export const URLSearchParams = globalThis.URLSearchParams;
export function parse(urlStr) { return new URL(urlStr); }
export function format(urlObj) { return String(urlObj); }
export function resolve(from, to) { return new URL(to, from).href; }
export default { URL, URLSearchParams, parse, format, resolve };
"#;

const OS_POLYFILL: &str = r#"// os polyfill (browser-safe)
export const platform = () => 'browser';
export const arch = () => 'wasm';
export const hostname = () => 'localhost';
export const tmpdir = () => '/tmp';
export const homedir = () => '/';
export const cpus = () => [];
export const totalmem = () => 0;
export const freemem = () => 0;
export const type = () => 'Browser';
export const release = () => '0';
export const EOL = '\n';
export default { platform, arch, hostname, tmpdir, homedir, cpus, totalmem, freemem, type, release, EOL };
"#;

const FS_POLYFILL: &str = r#"// fs polyfill — no-op in browser
export function readFileSync() { throw new Error('fs.readFileSync is not available in browser'); }
export function writeFileSync() { throw new Error('fs.writeFileSync is not available in browser'); }
export function existsSync() { return false; }
export function mkdirSync() {}
export function readdirSync() { return []; }
export function statSync() { return { isFile: () => false, isDirectory: () => false, size: 0 }; }
export const promises = {
  readFile: () => Promise.reject(new Error('fs.promises.readFile not available in browser')),
  writeFile: () => Promise.reject(new Error('fs.promises.writeFile not available in browser')),
  readdir: () => Promise.resolve([]),
  mkdir: () => Promise.resolve(),
  stat: () => Promise.reject(new Error('fs.promises.stat not available in browser')),
};
export default { readFileSync, writeFileSync, existsSync, mkdirSync, readdirSync, statSync, promises };
"#;

const HTTP_POLYFILL: &str = r#"// http polyfill — use fetch in browser
export function request() { throw new Error('Use fetch() in browser'); }
export function get() { throw new Error('Use fetch() in browser'); }
export const Agent = function() {};
export default { request, get, Agent };
"#;

const HTTPS_POLYFILL: &str = r#"// https polyfill — use fetch in browser
export function request() { throw new Error('Use fetch() in browser'); }
export function get() { throw new Error('Use fetch() in browser'); }
export const Agent = function() {};
export default { request, get, Agent };
"#;

const NET_POLYFILL: &str = r#"// net polyfill — not available in browser
export function createServer() { throw new Error('net is not available in browser'); }
export function connect() { throw new Error('net is not available in browser'); }
export default { createServer, connect };
"#;

const TLS_POLYFILL: &str = r#"// tls polyfill — not available in browser
export function connect() { throw new Error('tls is not available in browser'); }
export function createServer() { throw new Error('tls is not available in browser'); }
export default { connect, createServer };
"#;

const ZLIB_POLYFILL: &str = r#"// zlib polyfill — use DecompressionStream/CompressionStream
export function gzip() { throw new Error('Use CompressionStream in browser'); }
export function gunzip() { throw new Error('Use DecompressionStream in browser'); }
export function deflate() { throw new Error('Use CompressionStream in browser'); }
export function inflate() { throw new Error('Use DecompressionStream in browser'); }
export function brotliCompress() { throw new Error('Use CompressionStream in browser'); }
export function brotliDecompress() { throw new Error('Use DecompressionStream in browser'); }
export default { gzip, gunzip, deflate, inflate, brotliCompress, brotliDecompress };
"#;

const QUERYSTRING_POLYFILL: &str = r#"// querystring polyfill
export function parse(qs) {
  const result = {};
  if (!qs) return result;
  const pairs = qs.replace(/^\?/, '').split('&');
  for (const pair of pairs) {
    const [key, val] = pair.split('=');
    result[decodeURIComponent(key)] = val ? decodeURIComponent(val) : '';
  }
  return result;
}
export function stringify(obj) {
  return Object.entries(obj).map(([k, v]) => encodeURIComponent(k) + '=' + encodeURIComponent(v)).join('&');
}
export function escape(s) { return encodeURIComponent(s); }
export function unescape(s) { return decodeURIComponent(s); }
export default { parse, stringify, escape, unescape };
"#;

const STRING_DECODER_POLYFILL: &str = r#"// string_decoder polyfill
export class StringDecoder {
  constructor(encoding) { this._encoding = encoding || 'utf8'; }
  write(buf) { return new TextDecoder(this._encoding).decode(buf); }
  end(buf) { return buf ? this.write(buf) : ''; }
}
export default StringDecoder;
"#;

const TIMERS_POLYFILL: &str = r#"// timers polyfill — use browser timers
export const setTimeout = globalThis.setTimeout.bind(globalThis);
export const clearTimeout = globalThis.clearTimeout.bind(globalThis);
export const setInterval = globalThis.setInterval.bind(globalThis);
export const clearInterval = globalThis.clearInterval.bind(globalThis);
export const setImmediate = (fn, ...args) => setTimeout(fn, 0, ...args);
export const clearImmediate = (id) => clearTimeout(id);
export default { setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, clearImmediate };
"#;

const ASSERT_POLYFILL: &str = r#"// assert polyfill
export function ok(value, message) { if (!value) throw new Error(message || 'Assertion failed'); }
export function equal(a, b, message) { if (a !== b) throw new Error(message || `${a} !== ${b}`); }
export function deepEqual(a, b, message) { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(message || 'deepEqual failed'); }
export function throws(fn, message) { try { fn(); throw new Error(message || 'Expected to throw'); } catch(e) {} }
export function doesNotThrow(fn, message) { try { fn(); } catch(e) { throw new Error(message || 'Expected not to throw'); } }
export function fail(message) { throw new Error(message || 'Assertion failed'); }
export default { ok, equal, deepEqual, throws, doesNotThrow, fail };
"#;

const CHILD_PROCESS_POLYFILL: &str = r#"// child_process polyfill — not available in browser
export function exec() { throw new Error('child_process is not available in browser'); }
export function execSync() { throw new Error('child_process is not available in browser'); }
export function spawn() { throw new Error('child_process is not available in browser'); }
export function fork() { throw new Error('child_process is not available in browser'); }
export default { exec, execSync, spawn, fork };
"#;
