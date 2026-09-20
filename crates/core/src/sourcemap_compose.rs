//! Source map composition.
//!
//! When a plugin's `transform` hook rewrites a module and returns a source map
//! (`generated -> original`), the built-in transform that runs afterwards
//! produces a second map (`final -> plugin output`). Neither map alone points
//! at the file on disk; the correct final map is their composition
//! (`final -> plugin output -> original`).
//!
//! [`compose_source_maps`] does that on standard v3 JSON maps without any
//! external dependency (a small Base64-VLQ codec is included). It uses the
//! standard "greatest generated column <= lookup column" rule to trace each
//! outer mapping through the inner map; mappings that cannot be traced (the
//! inner map has no segment covering the position) are dropped rather than
//! guessed.

use serde_json::{Value, json};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_val(c: u8) -> Option<i64> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as i64),
        b'a'..=b'z' => Some((c - b'a') as i64 + 26),
        b'0'..=b'9' => Some((c - b'0') as i64 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode one comma-separated segment into its VLQ fields.
fn decode_vlq_segment(seg: &str) -> Option<Vec<i64>> {
    let mut out = Vec::with_capacity(5);
    let mut shift = 0u32;
    let mut acc: i64 = 0;
    for &c in seg.as_bytes() {
        let v = b64_val(c)?;
        acc += (v & 31).checked_shl(shift)?;
        if v & 32 != 0 {
            shift += 5;
            if shift > 60 {
                return None;
            }
        } else {
            let neg = acc & 1 == 1;
            let n = acc >> 1;
            out.push(if neg { -n } else { n });
            acc = 0;
            shift = 0;
        }
    }
    if shift != 0 { None } else { Some(out) }
}

fn encode_vlq(value: i64, out: &mut String) {
    let mut v: u64 = if value < 0 {
        (value.unsigned_abs() << 1) | 1
    } else {
        (value as u64) << 1
    };
    loop {
        let mut digit = (v & 31) as usize;
        v >>= 5;
        if v != 0 {
            digit |= 32;
        }
        out.push(B64[digit] as char);
        if v == 0 {
            break;
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Seg {
    gen_col: i64,
    /// (source index, line, column, optional name index)
    src: Option<(i64, i64, i64, Option<i64>)>,
}

/// Decode `mappings` into one segment list per generated line.
fn decode_mappings(mappings: &str) -> Option<Vec<Vec<Seg>>> {
    let mut lines = Vec::new();
    let (mut src, mut line, mut col, mut name) = (0i64, 0i64, 0i64, 0i64);
    for gen_line in mappings.split(';') {
        let mut gen_col = 0i64;
        let mut segs = Vec::new();
        if !gen_line.is_empty() {
            for raw in gen_line.split(',') {
                let f = decode_vlq_segment(raw)?;
                gen_col += *f.first()?;
                match f.len() {
                    1 => segs.push(Seg { gen_col, src: None }),
                    4 | 5 => {
                        src += f[1];
                        line += f[2];
                        col += f[3];
                        let n = if f.len() == 5 {
                            name += f[4];
                            Some(name)
                        } else {
                            None
                        };
                        segs.push(Seg {
                            gen_col,
                            src: Some((src, line, col, n)),
                        });
                    }
                    _ => return None,
                }
            }
        }
        lines.push(segs);
    }
    Some(lines)
}

struct Parsed {
    file: Option<String>,
    source_root: Option<String>,
    sources: Vec<Value>,
    sources_content: Option<Vec<Value>>,
    names: Vec<Value>,
    lines: Vec<Vec<Seg>>,
}

fn parse(json_map: &str) -> Option<Parsed> {
    let v: Value = serde_json::from_str(json_map).ok()?;
    let o = v.as_object()?;
    // Indexed (`sections`) maps are not flattened here.
    if o.contains_key("sections") {
        return None;
    }
    Some(Parsed {
        file: o.get("file").and_then(|f| f.as_str()).map(String::from),
        source_root: o
            .get("sourceRoot")
            .and_then(|f| f.as_str())
            .map(String::from),
        sources: o.get("sources")?.as_array()?.clone(),
        sources_content: o.get("sourcesContent").and_then(|c| c.as_array()).cloned(),
        names: o
            .get("names")
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default(),
        lines: decode_mappings(o.get("mappings")?.as_str()?)?,
    })
}

/// Compose `outer` (`final -> intermediate`) with `inner`
/// (`intermediate -> original`) into a `final -> original` map.
///
/// Returns `None` when either map cannot be parsed (or is an indexed map);
/// callers should then keep the outer map unchanged.
pub fn compose_source_maps(outer_json: &str, inner_json: &str) -> Option<String> {
    let outer = parse(outer_json)?;
    let inner = parse(inner_json)?;

    fn name_index(n: &Value, names: &mut Vec<Value>) -> i64 {
        if let Some(p) = names.iter().position(|x| x == n) {
            p as i64
        } else {
            names.push(n.clone());
            (names.len() - 1) as i64
        }
    }
    let mut names: Vec<Value> = inner.names.clone();

    let mut mappings = String::new();
    let (mut p_src, mut p_line, mut p_col, mut p_name) = (0i64, 0i64, 0i64, 0i64);

    for (li, segs) in outer.lines.iter().enumerate() {
        if li > 0 {
            mappings.push(';');
        }
        let mut p_gen = 0i64;
        let mut first = true;
        for seg in segs {
            let Some((_, o_line, o_col, o_name)) = seg.src else {
                continue;
            };
            // Trace through the inner map: last inner segment on `o_line`
            // whose generated column is <= `o_col`.
            let Some(inner_segs) = inner.lines.get(o_line as usize) else {
                continue;
            };
            let Some(hit) = inner_segs
                .iter()
                .rfind(|s| s.gen_col <= o_col)
                .and_then(|s| s.src)
            else {
                continue;
            };
            let (i_src, i_line, i_col, i_name) = hit;

            // Prefer the inner (original) name, else the outer one.
            let name = match (i_name, o_name) {
                (Some(n), _) => inner
                    .names
                    .get(n as usize)
                    .map(|v| name_index(v, &mut names)),
                (None, Some(n)) => outer
                    .names
                    .get(n as usize)
                    .map(|v| name_index(v, &mut names)),
                _ => None,
            };

            if !first {
                mappings.push(',');
            }
            first = false;
            encode_vlq(seg.gen_col - p_gen, &mut mappings);
            p_gen = seg.gen_col;
            encode_vlq(i_src - p_src, &mut mappings);
            p_src = i_src;
            encode_vlq(i_line - p_line, &mut mappings);
            p_line = i_line;
            encode_vlq(i_col - p_col, &mut mappings);
            p_col = i_col;
            if let Some(n) = name {
                encode_vlq(n - p_name, &mut mappings);
                p_name = n;
            }
        }
    }

    let mut out = json!({
        "version": 3,
        "sources": inner.sources,
        "names": names,
        "mappings": mappings,
    });
    if let Some(f) = outer.file.or(inner.file) {
        out["file"] = Value::String(f);
    }
    if let Some(r) = inner.source_root {
        out["sourceRoot"] = Value::String(r);
    }
    if let Some(c) = inner.sources_content {
        out["sourcesContent"] = Value::Array(c);
    }
    Some(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(fields: &[i64]) -> String {
        let mut s = String::new();
        for f in fields {
            encode_vlq(*f, &mut s);
        }
        s
    }

    #[test]
    fn vlq_round_trips() {
        for v in [0, 1, -1, 15, 16, -16, 1000, -123456] {
            let mut s = String::new();
            encode_vlq(v, &mut s);
            assert_eq!(decode_vlq_segment(&s).unwrap(), vec![v], "value {v}");
        }
    }

    #[test]
    fn composes_final_to_plugin_to_original() {
        // inner: plugin output line 0 col 0 -> original.ts (line 4, col 2);
        //        plugin output line 1 col 0 -> original.ts (line 5, col 0)
        let inner_mappings = format!("{};{}", enc(&[0, 0, 4, 2]), enc(&[0, 0, 1, -2]));
        let inner = json!({
            "version": 3, "sources": ["original.ts"], "sourcesContent": ["src"],
            "names": [], "mappings": inner_mappings
        })
        .to_string();
        // outer: final line 0 col 3 -> plugin output (line 1, col 0);
        //        final line 0 col 9 -> plugin output (line 0, col 5)  (col 5 falls in
        //        the segment that starts at col 0 -> maps to the same original spot)
        let outer_mappings = format!("{},{}", enc(&[3, 0, 1, 0]), enc(&[6, 0, -1, 5]));
        let outer = json!({
            "version": 3, "file": "out.js", "sources": ["plugin-output.ts"],
            "names": [], "mappings": outer_mappings
        })
        .to_string();

        let composed = compose_source_maps(&outer, &inner).unwrap();
        let v: Value = serde_json::from_str(&composed).unwrap();
        assert_eq!(v["sources"], json!(["original.ts"]));
        assert_eq!(v["sourcesContent"], json!(["src"]));
        assert_eq!(v["file"], json!("out.js"));

        let lines = decode_mappings(v["mappings"].as_str().unwrap()).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 2);
        // final col 3 -> original (line 5, col 0)
        assert_eq!(lines[0][0].gen_col, 3);
        assert_eq!(lines[0][0].src.unwrap(), (0, 5, 0, None));
        // final col 9 -> original (line 4, col 2)
        assert_eq!(lines[0][1].gen_col, 9);
        assert_eq!(lines[0][1].src.unwrap(), (0, 4, 2, None));
    }

    #[test]
    fn untraceable_positions_are_dropped_and_bad_input_is_none() {
        let inner = json!({"version":3,"sources":["o.ts"],"names":[],"mappings":enc(&[4,0,0,0])})
            .to_string();
        // outer points at plugin-output col 0, but the only inner segment starts at col 4.
        let outer = json!({"version":3,"sources":["p.ts"],"names":[],"mappings":enc(&[0,0,0,0])})
            .to_string();
        let composed: Value =
            serde_json::from_str(&compose_source_maps(&outer, &inner).unwrap()).unwrap();
        assert_eq!(composed["mappings"], json!(""));

        assert!(compose_source_maps("not json", &inner).is_none());
        assert!(
            compose_source_maps(&json!({"version":3,"sections":[]}).to_string(), &inner).is_none()
        );
    }
}
