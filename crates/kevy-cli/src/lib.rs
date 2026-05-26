//! CLI-shaped Reply formatter for the kevy-cli REPL.
//!
//! Only `format_reply` lives here — the protocol pieces (TCP connect, request
//! loop) live in the [`kevy-resp-client`](https://crates.io/crates/kevy-resp-client)
//! stone so they're reusable by integration tests / scripts / other tools.
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
        Reply::Nil => "(nil)".to_string(),
        Reply::Array(items) if items.is_empty() => "(empty array)".to_string(),
        Reply::Array(items) => {
            let pad = "   ".repeat(indent);
            items
                .iter()
                .enumerate()
                .map(|(i, it)| format!("{pad}{}) {}", i + 1, format_reply(it, indent + 1)))
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}
