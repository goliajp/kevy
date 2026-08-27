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

/// Backup / restore container support. See
/// [`backup::pack`] and [`backup::unpack`].
pub mod backup;

/// Migration toolchain (`export` / `import`). See
/// [`migrate::run_export`] and [`migrate::run_import`].
pub mod migrate;

/// Where a subcommand connects when the caller says nothing. Shared
/// rather than repeated: two copies of a default is a drift waiting to
/// be reported as a bug.
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// The port half of the same default.
pub const DEFAULT_PORT: u16 = 6379;

/// Prefix bulk ops + diagnostics (`copy-prefix` /
/// `delete-prefix` / `digest` / `diff` / `inspect`).
pub mod bulk;

/// `shadow` — run the old query and the new one side by side and
/// report where they disagree, in membership AND in order.
pub mod shadow;

/// `doctor` — every table's VERIFY counters, turned into an exit code
/// a cron can act on.
pub mod backfill_keys;
pub(crate) mod collections;
pub mod doctor;
pub mod lint;

/// Route the migration-playbook tools, which share a shape: they read
/// and report, none of them moves data, and each exits with its own
/// verdict. `None` when `args` names something else.
pub fn route_tool(args: &[String]) -> Option<std::process::ExitCode> {
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str)? {
        "doctor" => Some(doctor::run_doctor_cli(rest)),
        "shadow" => Some(shadow::run_shadow_cli(rest)),
        "lint" => Some(lint::run_lint_cli(rest)),
        "backfill-keys" => Some(backfill_keys::run_backfill_keys_cli(rest)),
        _ => None,
    }
}

/// Pretty-print a reply roughly the way `redis-cli` does. Arrays are
/// numbered + indented; bulk strings are quoted; nil shows as `(nil)`.
pub fn format_reply(reply: &Reply, indent: usize) -> String {
    match reply {
        Reply::Simple(s) => String::from_utf8_lossy(s).into_owned(),
        Reply::Error(s) | Reply::BlobError(s) => {
            format!("(error) {}", String::from_utf8_lossy(s))
        }
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
    }
}

#[cfg(test)]
mod format_reply_tests {
    use super::format_reply;
    use kevy_resp::Reply;

    fn f(r: &Reply) -> String {
        format_reply(r, 0)
    }

    /// One case per `Reply` variant. `format_reply` carried 79 never-executed
    /// regions — the largest single symbol in this crate — while being a pure
    /// function from a reply to a string, which is as cheap to test as code
    /// gets. The expectations are redis-cli's rendering, which is what the
    /// function's own comment says it follows.
    #[test]
    fn every_reply_variant_renders() {
        assert_eq!(f(&Reply::Simple(b"OK".to_vec())), "OK");
        assert_eq!(f(&Reply::Error(b"ERR nope".to_vec())), "(error) ERR nope");
        assert_eq!(f(&Reply::Int(-7)), "(integer) -7");
        assert_eq!(f(&Reply::Bulk(b"hi".to_vec())), "\"hi\"");
        assert_eq!(f(&Reply::Nil), "(nil)");
        assert_eq!(f(&Reply::Array(vec![])), "(empty array)");
        assert_eq!(f(&Reply::Double(1.5)), "(double) 1.5");
        assert_eq!(f(&Reply::Boolean(true)), "(boolean) t");
        assert_eq!(f(&Reply::Boolean(false)), "(boolean) f");
        assert_eq!(f(&Reply::BigNumber(b"123456789012345678901".to_vec())),
                   "(bignum) 123456789012345678901");

        // RESP3's second null and second error spelling render as their
        // RESP2 counterparts — a client must not be able to tell which
        // wire form it got from the printed line.
        assert_eq!(f(&Reply::Null), "(nil)");
        assert_eq!(f(&Reply::BlobError(b"ERR nope".to_vec())), "(error) ERR nope");

        assert_eq!(
            f(&Reply::Verbatim { fmt: *b"txt", data: b"hello".to_vec() }),
            "(verbatim/txt) \"hello\""
        );

        // Set and Push share the array arm; a set of one is still numbered.
        assert_eq!(f(&Reply::Set(vec![Reply::Int(4)])), "1) (integer) 4");
        assert_eq!(
            f(&Reply::Push(vec![Reply::Bulk(b"message".to_vec())])),
            "1) \"message\""
        );
    }

    /// Arrays number from one and nest by indent — the recursive arm, which
    /// a single flat array would leave unexercised.
    #[test]
    fn arrays_number_from_one_and_nest() {
        let flat = Reply::Array(vec![Reply::Int(1), Reply::Bulk(b"x".to_vec())]);
        assert_eq!(f(&flat), "1) (integer) 1\n2) \"x\"");

        let nested = Reply::Array(vec![Reply::Array(vec![Reply::Int(9)])]);
        // The inner element is padded by one level; the outer is not.
        assert_eq!(f(&nested), "1)    1) (integer) 9");
    }

    /// An empty map is not an empty array, and a populated one renders
    /// `key => value` rather than as two flat elements.
    #[test]
    fn maps_render_as_pairs() {
        assert_eq!(f(&Reply::Map(vec![])), "(empty map)");
        let m = Reply::Map(vec![(Reply::Bulk(b"k".to_vec()), Reply::Int(1))]);
        assert_eq!(f(&m), "1) \"k\" => (integer) 1");
    }
}
