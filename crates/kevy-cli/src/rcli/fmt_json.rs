//! The JSON and QUOTED-JSON formatters, with the
//! places redis-cli emits text that is not JSON corrected.

use super::format::{Doubles, Output, c_str};
use super::repr::repr;
use kevy_resp::Reply;

/// Render `r` as one JSON value.
pub(crate) fn json(r: &Reply, d: &mut Doubles<'_>, mode: Output, out: &mut Vec<u8>) {
    match r {
        // DEV-004: redis-cli prints `error:"…"`, which no JSON parser reads.
        Reply::Error(m) | Reply::BlobError(m) => {
            out.extend_from_slice(b"{\"error\":");
            string(c_str(m), mode, out);
            out.push(b'}');
        }
        Reply::Simple(b) | Reply::Bulk(b) | Reply::Verbatim { data: b, .. } => string(b, mode, out),
        Reply::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Reply::Double(v) => double(&d.next_text(*v), out),
        Reply::BigNumber(digits) => out.extend_from_slice(digits),
        Reply::Nil | Reply::Null => out.extend_from_slice(b"null"),
        Reply::Boolean(t) => out.extend_from_slice(if *t { b"true" } else { b"false" }),
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                json(item, d, mode, out);
                if i + 1 < items.len() {
                    out.push(b',');
                }
            }
            out.push(b']');
        }
        Reply::Map(pairs) => {
            out.push(b'{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                key(k, d, mode, out);
                out.push(b':');
                json(v, d, mode, out);
                if i + 1 < pairs.len() {
                    out.push(b',');
                }
            }
            out.push(b'}');
        }
    }
}

/// A map key: strings as themselves, anything else rendered and quoted.
fn key(k: &Reply, d: &mut Doubles<'_>, mode: Output, out: &mut Vec<u8>) {
    match k {
        Reply::Simple(b) | Reply::Bulk(b) | Reply::Verbatim { data: b, .. } => string(b, mode, out),
        Reply::Error(m) | Reply::BlobError(m) => string(c_str(m), mode, out),
        other => {
            let mut rendered = Vec::new();
            json(other, d, mode, &mut rendered);
            if rendered.first() == Some(&b'"') {
                out.extend_from_slice(&rendered);
            } else {
                string(&rendered, Output::Json, out);
            }
        }
    }
}

/// DEV-009: `inf`, `-inf` and `nan` are not JSON numbers, so they are
/// written as strings; every other double text is a valid JSON number.
fn double(text: &[u8], out: &mut Vec<u8>) {
    if matches!(text, b"inf" | b"-inf" | b"nan") {
        out.push(b'"');
        out.extend_from_slice(text);
        out.push(b'"');
    } else {
        out.extend_from_slice(text);
    }
}

/// A JSON string. `--json` escapes the bytes;
/// `--quoted-json` makes the string's *value* redis-cli's quoted repr, so
/// binary shows as `\xff` — and DEV-005: that value is then escaped as JSON
/// once, where redis-cli doubles backslashes and breaks on a quote.
fn string(bytes: &[u8], mode: Output, out: &mut Vec<u8>) {
    if mode == Output::QuotedJson {
        let quoted = repr(bytes);
        escape(&quoted[1..quoted.len() - 1], out);
    } else {
        escape(bytes, out);
    }
}

/// redis-cli's JSON string escaping: control bytes as `\u00XX`, the short
/// escapes, and every other byte — including 0x80 and up — as is.
fn escape(bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &b in bytes {
        match b {
            b'\\' | b'"' => out.extend_from_slice(&[b'\\', b]),
            b'\n' => out.extend_from_slice(b"\\n"),
            0x0c => out.extend_from_slice(b"\\f"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x00..=0x1f => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
            _ => out.push(b),
        }
    }
    out.push(b'"');
}
