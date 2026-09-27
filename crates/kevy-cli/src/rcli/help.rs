//! `--help`: kevy-cli's usage text (DEV-001 — it documents kevy-cli's tools
//! as well as the redis-cli options, so it cannot be redis-cli's bytes).

use super::docs::model::Docs;
use std::io::Write;
use std::rc::Rc;

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
       kevy-cli [OPTIONS] --kevy <tool> [tool args]

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
  --kevy <tool>      Run one of kevy's own tools (listed below) instead of
                     sending a command; everything after the tool name is
                     the tool's.
  --help             Output this help and exit.
  -v, --version      Output version and exit.

TLS is not implemented: --tls and its companion flags are refused.

",
);

impl super::session::Session {
    /// `help` / `?` typed as a command: answered here, never sent.
    pub(crate) fn print_help(&mut self, topic: &[Vec<u8>]) {
        let text = if topic.is_empty() {
            super::docs::help_text::overview()
        } else {
            super::docs::help_text::topic(&self.docs(), topic)
        };
        super::send::write_out(&text);
    }

    /// The command reference, read from the server once: `COMMAND DOCS` when
    /// it answers with a table, else kevy's own. A subscribed connection
    /// cannot be asked, so it gets kevy's own without keeping it.
    pub(crate) fn docs(&mut self) -> Rc<Docs> {
        if let Some(docs) = &self.docs {
            return Rc::clone(docs);
        }
        if self.pubsub_mode || self.monitor_mode {
            return Rc::new(Docs::offline());
        }
        if self.conn.is_none() {
            self.connect(super::session::Connect::Quiet);
        }
        let docs = Rc::new(self.ask_docs().unwrap_or_else(Docs::offline));
        self.docs = Some(Rc::clone(&docs));
        docs
    }

    fn ask_docs(&mut self) -> Option<Docs> {
        let conn = self.conn.as_mut()?;
        let mut asked =
            conn.send(&[b"COMMAND".to_vec(), b"DOCS".to_vec()]).and_then(|()| conn.read_reply());
        // A push that was already on its way is not the answer.
        while let Ok((kevy_resp::Reply::Push(_), _)) = &asked {
            asked = conn.read_reply();
        }
        match asked {
            Ok((reply, _)) => Docs::from_reply(&reply),
            Err(e) => {
                self.link_error = Some(e);
                None
            }
        }
    }
}

/// Where a `:` line came from; a line from the preferences file says so.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    Prompt,
    File,
}

/// `:set hints` / `:set nohints`, `:get pubsub` (valkey-cli's; DEV-018), and
/// the messages for anything else. `subscribed`: what `:get pubsub` reports.
pub(crate) fn preference(argv: &[Vec<u8>], origin: Origin, subscribed: bool) {
    let is = |i: usize, w: &str| argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(w.as_bytes()));
    let from: &[u8] = if origin == Origin::File { b".kevyclirc: " } else { b"" };
    let say = |parts: &[&[u8]]| super::send::write_out(&[&[from], parts].concat().concat());
    if is(0, ":set") && argv.len() >= 2 {
        if is(1, "hints") || is(1, "nohints") {
            super::session::set_hints(is(1, "hints"));
        } else {
            say(&[b"unknown kevy-cli preference '", &argv[1], b"'\n"]);
        }
    } else if is(0, ":get") && argv.len() >= 2 {
        if is(1, "pubsub") {
            super::send::write_out(if subscribed { b"1\n" } else { b"0\n" });
        } else {
            say(&[b"unknown kevy-cli get option '", &argv[1], b"'\n"]);
        }
    } else {
        say(&[b"unknown kevy-cli internal command '", &argv[0], b"'\n"]);
    }
}

/// Apply the preferences file: `KEVYCLI_RCFILE`, `VALKEYCLI_RCFILE` or
/// `REDISCLI_RCFILE` (the first non-empty one; `/dev/null` for none), else
/// `~/.kevyclirc`. Each line is split like a REPL line.
pub(crate) fn load_preferences() {
    let named = ["KEVYCLI_RCFILE", "VALKEYCLI_RCFILE", "REDISCLI_RCFILE"]
        .iter()
        .find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()));
    let path = match named {
        Some(p) if p == "/dev/null" => return,
        Some(p) => std::path::PathBuf::from(p),
        None => match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => std::path::PathBuf::from(home).join(".kevyclirc"),
            None => return,
        },
    };
    let Ok(text) = std::fs::read(path) else { return };
    for line in text.split(|&b| b == b'\n') {
        if let Some(argv) = super::splitargs::split_args(line)
            && !argv.is_empty()
        {
            preference(&argv, Origin::File, false);
        }
    }
}
