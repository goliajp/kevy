//! The STANDARD formatter: `cliFormatReplyTTY` (rc:1843-1976).

use super::format::{Doubles, c_str};
use super::repr::push_repr;
use kevy_resp::Reply;

/// Render `r` for a terminal. `prefix` is the indentation a nested element
/// inherits; the first element of an aggregate gets none, because its
/// parent already printed the index in front of it.
pub(crate) fn tty(r: &Reply, prefix: &[u8], d: &mut Doubles<'_>, out: &mut Vec<u8>) {
    match r {
        Reply::Error(m) | Reply::BlobError(m) => {
            out.extend_from_slice(b"(error) ");
            out.extend_from_slice(c_str(m));
            out.push(b'\n');
        }
        Reply::Simple(s) => {
            out.extend_from_slice(c_str(s));
            out.push(b'\n');
        }
        Reply::Int(n) => out.extend_from_slice(format!("(integer) {n}\n").as_bytes()),
        Reply::Double(v) => {
            out.extend_from_slice(b"(double) ");
            out.extend_from_slice(&d.next_text(*v));
            out.push(b'\n');
        }
        Reply::Bulk(b) => {
            push_repr(out, b);
            out.push(b'\n');
        }
        Reply::Verbatim { data, .. } => {
            out.extend_from_slice(data);
            out.push(b'\n');
        }
        Reply::Nil | Reply::Null => out.extend_from_slice(b"(nil)\n"),
        Reply::Boolean(t) => out.extend_from_slice(if *t { b"(true)\n" } else { b"(false)\n" }),
        // DEV-007: redis-cli exits 1 with `Unknown reply type: 13`.
        Reply::BigNumber(digits) => {
            out.extend_from_slice(b"(bignum) ");
            out.extend_from_slice(digits);
            out.push(b'\n');
        }
        Reply::Array(items) => sequence(items, b')', b"(empty array)\n", prefix, d, out),
        Reply::Push(items) => sequence(items, b')', b"(empty push)\n", prefix, d, out),
        Reply::Set(items) => sequence(items, b'~', b"(empty set)\n", prefix, d, out),
        Reply::Map(pairs) => map(pairs, prefix, d, out),
    }
}

/// Digits needed for the largest index, and the prefix nested elements get.
fn index_layout(count: usize, prefix: &[u8]) -> (usize, Vec<u8>) {
    let width = count.to_string().len();
    let mut nested = prefix.to_vec();
    nested.resize(prefix.len() + width + 2, b' ');
    (width, nested)
}

fn sequence(
    items: &[Reply],
    sep: u8,
    empty: &[u8],
    prefix: &[u8],
    d: &mut Doubles<'_>,
    out: &mut Vec<u8>,
) {
    if items.is_empty() {
        out.extend_from_slice(empty);
        return;
    }
    let (width, nested) = index_layout(items.len(), prefix);
    for (i, item) in items.iter().enumerate() {
        index(i, width, sep, prefix, out);
        tty(item, &nested, d, out);
    }
}

fn map(pairs: &[(Reply, Reply)], prefix: &[u8], d: &mut Doubles<'_>, out: &mut Vec<u8>) {
    if pairs.is_empty() {
        out.extend_from_slice(b"(empty hash)\n");
        return;
    }
    let (width, nested) = index_layout(pairs.len(), prefix);
    for (i, (k, v)) in pairs.iter().enumerate() {
        index(i, width, b'#', prefix, out);
        tty(k, &nested, d, out);
        out.pop(); // the key's trailing newline: `sdsrange(out,0,-2)`
        out.extend_from_slice(b" => ");
        if is_multiline(v) {
            out.push(b'\n');
            out.extend_from_slice(&nested);
        }
        tty(v, &nested, d, out);
    }
}

/// `%s%<width>u<sep> ` — the parent's prefix is skipped for element 0.
fn index(i: usize, width: usize, sep: u8, prefix: &[u8], out: &mut Vec<u8>) {
    if i > 0 {
        out.extend_from_slice(prefix);
    }
    out.extend_from_slice(format!("{:>width$}", i + 1).as_bytes());
    out.push(sep);
    out.push(b' ');
}

/// `cliIsMultilineValueTTY`.
fn is_multiline(r: &Reply) -> bool {
    match r {
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => match items.len() {
            0 => false,
            1 => is_multiline(&items[0]),
            _ => true,
        },
        Reply::Map(pairs) => match pairs.len() {
            0 => false,
            1 => is_multiline(&pairs[0].1),
            _ => true,
        },
        _ => false,
    }
}

/// A RESP3 client-side-caching invalidation push, as redis-cli detects it.
pub(crate) fn is_invalidate(r: &Reply) -> bool {
    matches!(r, Reply::Push(items) if items.len() == 2
        && matches!(&items[0], Reply::Bulk(s) if s.starts_with(b"invalidate"))
        && matches!(items[1], Reply::Array(_)))
}

/// `cliFormatInvalidateTTY`: `-> invalidate: 'k1', 'k2'`.
pub(crate) fn invalidate_tty(r: &Reply) -> Vec<u8> {
    let mut out = b"-> invalidate: ".to_vec();
    if let Reply::Push(items) = r
        && let Reply::Array(keys) = &items[1]
    {
        for (i, key) in keys.iter().enumerate() {
            let text = match key {
                Reply::Bulk(b) => c_str(b),
                _ => b"",
            };
            out.push(b'\'');
            out.extend_from_slice(text);
            out.push(b'\'');
            if i + 1 < keys.len() {
                out.extend_from_slice(b", ");
            }
        }
    }
    out.push(b'\n');
    out
}
