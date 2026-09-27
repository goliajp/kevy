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

/// kevy-cli's own tools, the second half of `--help`.
pub(crate) const TOOLS_HELP: &str = "\
KEVY TOOLS: kevy-cli [connection options] --kevy <tool> [args]
    (in the REPL: \\<tool> [args]; a bare word is always a server command)

  catalog
    tables [pattern]                            TABLE.LIST as rows
    indexes [table|pattern]                     IDX.LIST, with each index's table
    views [pattern]                             VIEW.LIST
    describe[+] <table|index|view>              columns and access paths, an
                                                index's fields, a view's tree
                                                (+ runs its VERIFY)
    show-create <name> [--as kevy|sql]          the declaration that recreates it

  query
    query [--all] [--max-rows n] <IDX.QUERY|VIEW.QUERY …>
                                                rows, following the cursor
    explain <index> <shape…> | view <name>      the path a query takes
    explain --analyze <query…>                  run it; measured by this client
    advise                                      paths refused queries asked for
    sql run [--max-rows n] 'SELECT …'           one SELECT over declared paths,
                                                sent as the IDX.QUERY for it
    sql compile <file.sql> [--apply]            CREATE TABLE/INDEX/VIEW as
                                                TABLE.DECLARE / VIEW.CREATE and
                                                query cards (--apply declares)
    sql plan <file.sql>                         what becomes of every query
    sql eval '<SELECT …>' [--at <ts>]           fold a table-free SELECT

  data, one level per pair
    backup --data-dir <d> --to <f>              a data directory, offline
    restore --from <f> --to <d>
    export [--prefix p] <file>                  the keyspace as RESP, online
    import [--resume] [--strict] <file>
    dump --schema [--table t]… [--as kevy|sql]  declarations as a script
    dump --all <dir>                            + each table's declared columns
    load <dir>                                  rows, declarations, wait-ready,
                                                doctor
    export-csv (--prefix p --columns a,b | --table t) [--via \"IDX.QUERY …\"] <f|->
    import-csv <f> (--prefix p --pk c | --table t | --key-column c)
               (--header|--columns a,b)
    copy-prefix [--rate n] <from> <to>
    delete-prefix [--rate n] [--dry-run] <prefix>

  checks
    digest <prefix>                             hash a prefix
    diff <other host:port|redis://…> <prefix>…  compare it with another server
    inspect <prefix>                            sample keys: types and sizes
    doctor [--warn-is-failure] [--indexes] [--views]
                                                VERIFY every table (and bare
                                                index, view); exit code answers
    lint overlap --prefix <p:>                  a name under more than one owner?
    lint columns <table>                        column pairs that agree on most rows
    backfill-keys --from-index <k> --from-prefix <p:> --from-file <f>
                                                the union of every source
    shadow --old <cmd> --new <cmd>              old and new read paths, compared
                                                in membership and order
    wait-ready [--index n|--table t|--all] [--timeout s]
    status                                      server, role, keys, catalogs

  running
    run [-f file]… [-c cmd]… [--force] [--echo] [--atomic]
                                                exit 3 on an error reply, 2 on a
                                                lost link
    watch <seconds> [count] <command…>
    feed follow [--prefix p]… [--shard n|all] [--from tail|gen:off]
                [--checkpoint f] [--as json|resp] [--on-resync stop|jump]

  Rows: --format table|tsv|csv|json, --no-header, --null s, --expanded,
        --timing (a table on a terminal, tsv when piped).
  Until 7.0 the 6.4 forms (kevy-cli doctor -p 6004 …) still run, with a
  deprecation line.

EXAMPLES:
    kevy-cli                            # REPL against 127.0.0.1:6379
    kevy-cli -p 6004                    # REPL against kevy default port
    kevy-cli -h prod.internal ping      # one-shot PING
    kevy-cli -p 6004 set greet hello    # one-shot SET, exits 0

    # move a keyspace, and prove it arrived
    kevy-cli -p 6379 --kevy export --prefix user: dump.resp
    kevy-cli -p 6380 --kevy import --strict dump.resp
    kevy-cli -p 6379 --kevy diff 127.0.0.1:6380 user:

Docs: https://github.com/goliajp/kevy
";

/// `kevy-cli [options] [command]`: the redis-cli half of the binary.
///
/// # Examples
///
/// ```
/// // Parsing refuses what redis-cli refuses, with redis-cli's exit code.
/// assert_eq!(kevy_cli::rcli::run(&[b"-2".to_vec(), b"-3".to_vec()]), 1);
/// ```
pub mod rcli;

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
pub mod link;
pub mod lint;
mod tools;

/// Route a tool kevy-cli shipped as a bare word before `--kevy` (`sql`,
/// `export`, `import`, `backup`, `restore`, `doctor`, `shadow`, `lint`,
/// `backfill-keys`, `copy-prefix`, `delete-prefix`, `digest`, `diff`,
/// `inspect`): kept through 6.x with a deprecation line, removed in 7.0.
/// `None` when `args` names something else — a server command.
pub fn route_tool(args: &[String]) -> Option<std::process::ExitCode> {
    tools::bare::route(args)
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
        assert_eq!(
            f(&Reply::BigNumber(b"123456789012345678901".to_vec())),
            "(bignum) 123456789012345678901"
        );

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
        assert_eq!(f(&Reply::Push(vec![Reply::Bulk(b"message".to_vec())])), "1) \"message\"");
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
