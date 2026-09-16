//! `--help`: kevy-cli's usage text (DEV-001 — it documents kevy-cli's tools
//! as well as the redis-cli options, so it cannot be redis-cli's bytes).

use std::io::Write;

/// Print usage to stdout (`code` 0) or stderr (anything else); return `code`.
pub(crate) fn usage(code: u8) -> u8 {
    let text = format!("{OPTIONS_HELP}{}", crate::TOOLS_HELP);
    let _ = if code == 0 {
        // nothing to report to if the usage text cannot be written
        std::io::stdout().write_all(text.as_bytes())
    } else {
        std::io::stderr().write_all(text.as_bytes())
    };
    code
}

/// The redis-cli half of the help text.
const OPTIONS_HELP: &str = concat!(
    "kevy-cli ",
    env!("CARGO_PKG_VERSION"),
    " — a redis-cli for kevy or any RESP server, plus kevy's tools.

Usage: kevy-cli [OPTIONS] [cmd [arg [arg ...]]]
       kevy-cli <tool> [tool options]

  -h <hostname>      Server hostname (default: 127.0.0.1).
  -p <port>          Server port (default: 6379).
  -t <timeout>       Server connection timeout in seconds (decimals allowed).
  -s <socket>        Server socket (overrides hostname and port).
  -a <password>      Password to use when connecting to the server.
                     The KEVYCLI_AUTH, VALKEYCLI_AUTH or REDISCLI_AUTH
                     environment variable is used when this is not given.
  --pass <password>  Alias of -a.
  --user <username>  Used to send ACL style 'AUTH username pass'. Needs -a.
  --askpass          Force user to input password with mask from STDIN.
  -u <uri>           Server URI: redis://[[user]:password@]host[:port][/db]
                     (valkey:// is accepted too).
  -r <repeat>        Execute specified command N times (negative = forever).
  -i <interval>      When -r is used, waits <interval> seconds per command.
  -n <db>            Database number.
  -2                 Start session in RESP2 protocol mode.
  -3                 Start session in RESP3 protocol mode.
  -x                 Read last argument from STDIN.
  -X <tag>           Read <tag> argument from STDIN.
  -d <delimiter>     Delimiter between response bulks for raw formatting.
  -D <delimiter>     Delimiter between responses for raw formatting.
  -c                 Enable cluster mode (follow -ASK and -MOVED redirections).
  -e                 Return exit error code when command execution fails.
  -4 / -6            Prefer IPv4 / IPv6 on DNS lookup.
  --raw              Use raw formatting for replies (default when STDOUT is
                     not a tty).
  --no-raw           Force formatted output even when STDOUT is not a tty.
  --quoted-input     Force input to be handled as quoted strings.
  --csv              Output in CSV format.
  --json             Output in JSON format (valid JSON, including errors).
  --quoted-json      Same as --json, but produce ASCII-safe quoted strings.
  --show-pushes <yn> Whether to print RESP3 PUSH messages.
  --name <name>      Set the client name on connect.
  --no-auth-warning  Don't show warning message when using password on
                     command line interface.
  --verbose          Verbose mode.
  --help             Output this help and exit.
  -v, --version      Output version and exit.

TLS is not implemented: --tls and its companion flags are refused.

",
);

/// `help` / `?` typed as a command. The command reference is P1 (RC-072,
/// RC-148–151); until it lands this says so rather than printing nothing.
pub(crate) fn command_help(_topic: &[Vec<u8>]) {
    super::session::eprint_bytes(&[b"kevy-cli: help for commands is not implemented yet\n"]);
}

/// `:set hints` / `:set nohints` and the messages for anything else
/// (`cliSetPreferences`). Hints themselves are P1.
pub(crate) fn preference(argv: &[Vec<u8>]) {
    let is = |i: usize, w: &str| argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(w.as_bytes()));
    if is(0, ":set") && argv.len() >= 2 {
        if !(is(1, "hints") || is(1, "nohints")) {
            super::send::write_out(
                &[b"unknown kevy-cli preference '", argv[1].as_slice(), b"'\n"].concat(),
            );
        }
    } else {
        super::send::write_out(
            &[b"unknown kevy-cli internal command '", argv[0].as_slice(), b"'\n"].concat(),
        );
    }
}
