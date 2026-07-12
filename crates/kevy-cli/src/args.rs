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
        Config {
            host,
            port,
            command,
        }
    }
}

/// A leading token that looks like an option (`-x` / `--xyz`) rather
/// than a negative-number argument.
fn command_start_is_flaglike(tok: &str) -> bool {
    tok.len() > 1 && !tok[1..2].chars().next().is_some_and(|c| c.is_ascii_digit())
}
