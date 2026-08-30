//! CLI argument surface: help text + the leading-flag parser.
//! Split from main.rs (500-LOC rule).

use crate::{DEFAULT_HOST, DEFAULT_PORT};

pub(crate) fn print_help() {
    let v = env!("CARGO_PKG_VERSION");
    println!(
        "\
kevy-cli {v} — redis-cli-style REPL for kevy or any RESP server.

USAGE:
    kevy-cli [-h <host>] [-p <port>] [command [args ...]]
    kevy-cli <tool> [options]

OPTIONS:
    -h <host>           Server hostname (default: 127.0.0.1)
    -p <port>           Server port (default: 6379)
    --help              Show this help and exit
    -V, --version       Print version and exit

With a trailing command, runs once and exits non-zero on a RESP error.
Without a command, opens an interactive REPL (Ctrl-D / `quit` / `exit` to leave).
"
    );
    print_help_tools();
}

/// The tool-subcommand half of `--help` (split: fn ≤ 50 LOC rule).
fn print_help_tools() {
    println!(
        "\
SQL COMPILER (declaration-time only — never per-query):
    sql compile <file.sql>                      compile CREATE TABLE/INDEX/VIEW
                                                into TABLE.DECLARE / VIEW.CREATE
                                                commands + IDX.QUERY query cards
    sql compile <file.sql> --apply --url <h:p>  additionally run the commands
                                                against a server (exits non-zero
                                                on any error reply)
    sql plan <file.sql>                         what becomes of every query:
                                                which path serves it, or the
                                                CREATE INDEX it needs (no server)
"
    );
    print_help_migration_day();
}

/// The migration-playbook tools, listed apart because they share a
/// property worth seeing: each reads, reports, and moves nothing.
fn print_help_migration_day() {
    println!(
        "\
MIGRATION DAY (read and report; none of these moves data):
    lint overlap --prefix <p:>                  does a name live under more than
                                                one owner? then no column can
                                                carry that dimension
    lint columns <table>                        column pairs that agree on most
                                                rows — one column copied to get
                                                a second sort order
    backfill-keys --from-index <k> --from-prefix <p:> --from-file <f>
                                                the union of every source that
                                                can name an item, and how many
                                                names only one source had
    shadow --old <cmd> --new <cmd>              compare the old read path with
                                                the new one, in membership AND
                                                in order, before cutting over
    doctor [--warn-is-failure]                  TABLE.VERIFY every table and
                                                answer with an exit code

MIGRATION TOOLS:
    export  -p <port> [--prefix <p>] <file>     dump the keyspace to a RESP file
    import  -p <port> [--strict|--resume] <f>   load one back in
    digest  -p <port> <prefix>                  hash a prefix, to prove two
                                                servers agree
    diff    <hostA:port> <hostB:port> <prefix>… report where they do not

    copy-prefix    -p <port> [--rate n] <from> <to>
    delete-prefix  -p <port> [--rate n] [--dry-run] <prefix>
    inspect -p <port> <key>                     type, size and TTL of one key

EXAMPLES:
    kevy-cli                            # REPL against 127.0.0.1:6379
    kevy-cli -p 6004                    # REPL against kevy default port
    kevy-cli -h prod.internal ping      # one-shot PING
    kevy-cli -p 6004 set greet hello    # one-shot SET, exits 0

    # move a keyspace, and prove it arrived
    kevy-cli export -p 6379 --prefix user: dump.resp
    kevy-cli import -p 6380 --strict dump.resp
    kevy-cli digest -p 6379 user: && kevy-cli digest -p 6380 user:

Docs: https://github.com/goliajp/kevy"
    );
}

/// Parsed command-line configuration.
pub(crate) struct Config {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) command: Vec<Vec<u8>>,
}

impl Config {
    pub(crate) fn from_args(args: impl Iterator<Item = String>) -> Config {
        let mut host = DEFAULT_HOST.to_string();
        let mut port = DEFAULT_PORT;
        let mut command = Vec::new();
        let mut args = args.peekable();
        // Leading -h/-p flags, then everything else is the command.
        while let Some(arg) = args.peek() {
            match arg.as_str() {
                "-h" => {
                    args.next();
                    if let Some(h) = args.next() {
                        host = h;
                    }
                }
                "-p" => {
                    args.next();
                    if let Some(p) = args.next().and_then(|s| s.parse().ok()) {
                        port = p;
                    }
                }
                other if other.starts_with('-') && command_start_is_flaglike(other) => {
                    // An unrecognized leading flag would otherwise be
                    // sent to the server as a COMMAND — whose 'unknown
                    // command' reply points users at the wrong layer
                    // (reported by a downstream user). Fail HERE, by name.
                    eprintln!(
                        "kevy-cli: unknown option '{other}' (options go before the command; see kevy-cli --help)"
                    );
                    std::process::exit(2);
                }
                _ => break,
            }
        }
        command.extend(args.map(String::into_bytes));
        Config { host, port, command }
    }
}

/// A leading token that looks like an option (`-x` / `--xyz`) rather
/// than a negative-number argument.
fn command_start_is_flaglike(tok: &str) -> bool {
    tok.len() > 1 && !tok[1..2].chars().next().is_some_and(|c| c.is_ascii_digit())
}
