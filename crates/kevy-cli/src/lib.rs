//! CLI-shaped Reply formatter for the kevy-cli REPL.
//!
//! Only `format_reply` lives here — the protocol pieces (TCP connect, request
//! loop) live in the [`kevy-resp-client`](https://crates.io/crates/kevy-resp-client)
//! crate so they're reusable by integration tests / scripts / other tools.
//! This file is the CLI-specific bit (how a redis-cli user expects bulk
//! strings quoted, arrays numbered, nil shown as `(nil)`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub use kevy_resp::Reply;

/// Pretty-print a reply roughly the way `redis-cli` does. Arrays are
/// numbered + indented; bulk strings are quoted; nil shows as `(nil)`.
pub fn format_reply(reply: &Reply, indent: usize) -> String {
    match reply {
        Reply::Simple(s) => String::from_utf8_lossy(s).into_owned(),
        Reply::Error(s) => format!("(error) {}", String::from_utf8_lossy(s)),
        Reply::Int(n) => format!("(integer) {n}"),
        Reply::Bulk(b) => format!("\"{}\"", String::from_utf8_lossy(b)),
        Reply::Nil | Reply::Null => "(nil)".to_string(),
        Reply::Array(items) if items.is_empty() => "(empty array)".to_string(),
        Reply::Array(items) | Reply::Set(items) | Reply::Push(items) => {
            let pad = "   ".repeat(indent);
            items
                .iter()
                .enumerate()
                .map(|(i, it)| format!("{pad}{}) {}", i + 1, format_reply(it, indent + 1)))
                .collect::<Vec<_>>()
                .join("\n")
        }
        // RESP3 additions: format the same way redis-cli does today.
        Reply::Map(pairs) if pairs.is_empty() => "(empty map)".to_string(),
        Reply::Map(pairs) => {
            let pad = "   ".repeat(indent);
            pairs
                .iter()
                .enumerate()
                .map(|(i, (k, v))| {
                    format!(
                        "{pad}{}) {} => {}",
                        i + 1,
                        format_reply(k, indent + 1),
                        format_reply(v, indent + 1)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        Reply::Double(v) => format!("(double) {v}"),
        Reply::Boolean(b) => format!("(boolean) {}", if *b { "t" } else { "f" }),
        Reply::Verbatim { fmt, data } => format!(
            "(verbatim/{}) \"{}\"",
            String::from_utf8_lossy(fmt),
            String::from_utf8_lossy(data)
        ),
        Reply::BigNumber(s) => format!("(bignum) {}", String::from_utf8_lossy(s)),
        Reply::BlobError(s) => format!("(error) {}", String::from_utf8_lossy(s)),
    }
}
