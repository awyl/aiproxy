//! Byte-faithful request-body helpers.
//!
//! Upstreams (e.g. MiniMax passive prompt caching) can be sensitive to the
//! exact serialization of the request body. Parsing into `serde_json::Value`
//! and re-serializing alphabetizes object keys (BTreeMap) and reformats
//! numbers, producing bytes the client never sent. These helpers patch the
//! one field we must change — the top-level `model` id, prefix-stripped —
//! while keeping every other byte exactly as the client wrote it.

/// Replace the value of the top-level `"model"` key in a JSON object without
/// disturbing any other byte: key order, whitespace, escapes and number
/// formatting are preserved. Nested `"model"` keys (inside messages, tools,
/// etc.) are never touched. Returns `None` when `raw` is not a JSON object
/// with a top-level string-valued `"model"` key.
pub fn replace_model_field(raw: &[u8], new_model: &str) -> Option<Vec<u8>> {
    let mut i = skip_ws(raw, 0);
    if raw.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(raw, i + 1);
    // Empty object.
    if raw.get(i) == Some(&b'}') {
        return None;
    }
    loop {
        // Entry key: a JSON string.
        let key = read_string(raw, i)?;
        let key_end = key.1;
        i = skip_ws(raw, key_end);
        if raw.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(raw, i + 1);
        let val = read_value(raw, i)?;

        if &raw[key.0..key.1] == b"\"model\"" {
            let mut out = Vec::with_capacity(raw.len() + new_model.len());
            out.extend_from_slice(&raw[..val.0]);
            out.push(b'"');
            out.extend_from_slice(&escape_json_string(new_model));
            out.push(b'"');
            out.extend_from_slice(&raw[val.1..]);
            return Some(out);
        }

        i = skip_ws(raw, val.1);
        match raw.get(i) {
            Some(b',') => i = skip_ws(raw, i + 1),
            Some(b'}') => return None,
            _ => return None,
        }
    }
}

fn skip_ws(raw: &[u8], mut i: usize) -> usize {
    while let Some(&b) = raw.get(i) {
        if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// A `(start, end)` span of a JSON string literal including both quotes.
type Span = (usize, usize);

/// Read a JSON string literal starting at `i`; returns its span.
fn read_string(raw: &[u8], i: usize) -> Option<Span> {
    if raw.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while let Some(&b) = raw.get(j) {
        match b {
            b'\\' => j += 2,
            b'"' => return Some((i, j + 1)),
            _ => j += 1,
        }
    }
    None
}

/// Read one JSON value (string, number, bool, null, object, array) starting
/// at `i`; returns its span. Object/array scanning is depth-aware and
/// string-aware so braces inside strings never confuse it.
fn read_value(raw: &[u8], i: usize) -> Option<Span> {
    match raw.get(i)? {
        b'"' => read_string(raw, i),
        b'{' | b'[' => {
            let open = raw[i];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 0usize;
            let mut j = i;
            while let Some(&b) = raw.get(j) {
                match b {
                    b'"' => {
                        let span = read_string(raw, j)?;
                        j = span.1;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some((i, j + 1));
                        }
                        let _ = close;
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        b't' => raw.get(i..i + 4).filter(|s| s == b"true").map(|_| (i, i + 4)),
        b'f' => raw.get(i..i + 5).filter(|s| s == b"false").map(|_| (i, i + 5)),
        b'n' => raw.get(i..i + 4).filter(|s| s == b"null").map(|_| (i, i + 4)),
        _ => {
            // Number: consume until a delimiter.
            let mut j = i;
            while let Some(&b) = raw.get(j) {
                if b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E') {
                    j += 1;
                } else {
                    break;
                }
            }
            if j == i {
                None
            } else {
                Some((i, j))
            }
        }
    }
}

/// Escape a string for embedding as a JSON string value (inner content only).
fn escape_json_string(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes()),
            c => out.extend_from_slice(c.to_string().as_bytes()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_top_level_model_preserving_order_and_whitespace() {
        let raw = br#"{ "stream": true, "messages": [{"role":"user"}], "model": "alpha/m1", "max_tokens": 100 }"#;
        let out = replace_model_field(raw, "m1").unwrap();
        let expected = br#"{ "stream": true, "messages": [{"role":"user"}], "model": "m1", "max_tokens": 100 }"#;
        assert_eq!(out, expected.as_slice());
    }

    #[test]
    fn compact_body_with_model_first() {
        let raw = br#"{"model":"alpha/m1","messages":[]}"#;
        let out = replace_model_field(raw, "m1").unwrap();
        assert_eq!(out, br#"{"model":"m1","messages":[]}"#.to_vec().as_slice());
    }

    #[test]
    fn ignores_nested_model_keys() {
        let raw = br#"{"messages":[{"role":"user","content":"set \"model\": \"evil\" here"}],"model":"alpha/m1"}"#;
        let out = replace_model_field(raw, "m1").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""content":"set \"model\": \"evil\" here""#));
        assert!(s.contains(r#""model": "m1""#) || s.contains(r#""model":"m1""#));
    }

    #[test]
    fn nested_object_containing_model_key_is_untouched() {
        let raw = br#"{"response_format":{"model":"evil"},"model":"alpha/m1"}"#;
        let out = replace_model_field(raw, "m1").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#"{"model":"evil"}"#));
        assert!(s.contains(r#""model": "m1""#) || s.contains(r#""model":"m1""#));
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_depth_tracking() {
        let raw = br#"{"messages":"{not json [really","model":"alpha/m1"}"#;
        let out = replace_model_field(raw, "m1").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with(r#"{"messages":"{not json [really","model":"#));
    }

    #[test]
    fn escapes_new_model_id() {
        let raw = br#"{"model":"a/m"}"#;
        let out = replace_model_field(raw, "we\"ird\\n").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"model\":\"we\\\"ird\\\\n\"}"
        );
    }

    #[test]
    fn none_for_non_object_or_missing_model() {
        assert!(replace_model_field(b"[1,2]", "m").is_none());
        assert!(replace_model_field(br#"{"a":1}"#, "m").is_none());
        assert!(replace_model_field(b"", "m").is_none());
    }

    #[test]
    fn ignores_model_key_inside_tool_definitions() {
        let raw = br#"{"tools":[{"type":"function","function":{"parameters":{"properties":{"model":{"type":"string"}}}}}],"model":"alpha/m1"}"#;
        let out = replace_model_field(raw, "m1").unwrap();
        let s = String::from_utf8(out).unwrap();
        // nested "model" inside tool params must be untouched
        assert!(s.contains(r#""model":{"type":"string"}"#));
        // top-level "model" replaced
        assert!(s.contains(r#""model":"m1""#));
    }

    #[test]
    fn ignores_model_value_in_array_element() {
        let raw = br#"{"data":[{"model":"gpt-4o"},{"model":"claude-4"}],"model":"alpha/m1"}"#;
        let out = replace_model_field(raw, "m1").unwrap();
        let s = String::from_utf8(out).unwrap();
        // array elements untouched
        assert!(s.contains(r#""model":"gpt-4o""#));
        assert!(s.contains(r#""model":"claude-4""#));
        // top-level replaced
        assert!(s.contains(r#""model":"m1""#));
    }

    #[test]
    fn realistic_openai_request_body() {
        let raw = br#"{"model":"openai/gpt-4o","messages":[{"role":"system","content":"You are helpful."},{"role":"user","content":"hi"}],"stream":true,"temperature":0.7,"max_tokens":4096}"#;
        let out = replace_model_field(raw, "gpt-4o").unwrap();
        let s = String::from_utf8(out).unwrap();
        // model replaced
        assert!(s.contains(r#""model":"gpt-4o""#));
        // everything else byte-identical
        assert!(s.contains(r#""role":"system""#));
        assert!(s.contains(r#""stream":true"#));
        assert!(s.contains(r#""temperature":0.7"#));
        assert!(s.contains(r#""max_tokens":4096"#));
    }

    #[test]
    fn realistic_anthropic_request_body() {
        let raw = br#"{"model":"anthropic/claude-sonnet-4","max_tokens":8192,"messages":[{"role":"user","content":"hello"}],"stream":true,"thinking":{"type":"enabled","budget_tokens":4096}}"#;
        let out = replace_model_field(raw, "claude-sonnet-4").unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""model":"claude-sonnet-4""#));
        assert!(s.contains(r#""budget_tokens":4096"#));
        assert!(s.contains(r#""type":"enabled""#));
    }
}
