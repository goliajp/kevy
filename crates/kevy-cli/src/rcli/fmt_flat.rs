//! The RAW and CSV formatters.

use super::format::{Doubles, c_str};
use super::repr::push_repr;
use kevy_resp::Reply;

/// RAW: bytes as sent; aggregates flattened with `-d` between elements.
pub(crate) fn raw(r: &Reply, d: &mut Doubles<'_>, delim: &[u8], out: &mut Vec<u8>) {
    match r {
        Reply::Nil | Reply::Null => {}
        Reply::Error(m) | Reply::BlobError(m) => {
            out.extend_from_slice(m);
            out.push(b'\n');
        }
        Reply::Simple(b) | Reply::Bulk(b) | Reply::Verbatim { data: b, .. } => {
            out.extend_from_slice(b)
        }
        Reply::Boolean(t) => out.extend_from_slice(if *t { b"(true)" } else { b"(false)" }),
        Reply::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Reply::Double(v) => out.extend_from_slice(&d.next_text(*v)),
        // DEV-007: redis-cli exits 1 on a big number.
        Reply::BigNumber(digits) => out.extend_from_slice(digits),
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(delim);
                }
                raw(item, d, delim, out);
            }
        }
        Reply::Map(pairs) => {
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.extend_from_slice(delim);
                }
                raw(k, d, delim, out);
                out.push(b' ');
                raw(v, d, delim, out);
            }
        }
    }
}

/// CSV: quoted strings, `NULL`, aggregates flattened with commas.
pub(crate) fn csv(r: &Reply, d: &mut Doubles<'_>, out: &mut Vec<u8>) {
    match r {
        Reply::Error(m) | Reply::BlobError(m) => {
            out.extend_from_slice(b"ERROR,");
            push_repr(out, c_str(m));
        }
        Reply::Simple(b) | Reply::Bulk(b) | Reply::Verbatim { data: b, .. } => push_repr(out, b),
        Reply::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Reply::Double(v) => out.extend_from_slice(&d.next_text(*v)),
        Reply::BigNumber(digits) => out.extend_from_slice(digits),
        Reply::Nil | Reply::Null => out.extend_from_slice(b"NULL"),
        Reply::Boolean(t) => out.extend_from_slice(if *t { b"true" } else { b"false" }),
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
            for (i, item) in items.iter().enumerate() {
                csv(item, d, out);
                if i + 1 < items.len() {
                    out.push(b',');
                }
            }
        }
        Reply::Map(pairs) => {
            for (i, (k, v)) in pairs.iter().enumerate() {
                csv(k, d, out);
                out.push(b',');
                csv(v, d, out);
                if i + 1 < pairs.len() {
                    out.push(b',');
                }
            }
        }
    }
}
