//! The Lua debugger, from the client's side: `SCRIPT DEBUG yes|sync` arms it,
//! the next EVAL starts a session whose replies are debugger status lines,
//! and a `<endsession>` line ends it.

use super::format::Output;
use super::session::Session;
use kevy_resp::Reply;

/// Where a debugging session is.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ldb {
    /// SCRIPT DEBUG asked for one: the next EVAL starts it.
    pub(crate) armed: bool,
    /// Started by --ldb-sync-mode: changes the script makes are kept.
    pub(crate) sync: bool,
    /// A session is running: the prompt and line handling are the debugger's.
    pub(crate) active: bool,
    /// The session just ended; the EVAL's own reply is still to be read.
    pub(crate) ended: bool,
    /// `restart` was typed: run the script again in a new session.
    pub(crate) restart: bool,
}

impl Session {
    /// Note what a command about to be sent does to the debugger.
    pub(crate) fn ldb_before(&mut self, argv: &[Vec<u8>]) {
        let is =
            |i: usize, w: &str| argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(w.as_bytes()));
        if is(0, "script") && is(1, "debug") && argv.len() >= 3 {
            self.ldb.armed = is(2, "yes") || is(2, "sync");
        }
        if is(0, "eval") && self.ldb.armed {
            self.ldb.active = true;
            self.opts.output = Output::Raw;
        }
    }

    /// A reply during a session: status lines coloured by what they are,
    /// and `<endsession>` taken as the end (it prints nothing).
    pub(crate) fn ldb_reply(&mut self, reply: Reply) -> Reply {
        match reply {
            Reply::Simple(line) if line.starts_with(b"<endsession>") => {
                self.ldb = Ldb { ended: true, sync: self.ldb.sync, ..Ldb::default() };
                self.opts.output = Output::Standard;
                Reply::Simple(Vec::new())
            }
            Reply::Simple(line) => Reply::Simple(colored(&line)),
            Reply::Array(items) => {
                Reply::Array(items.into_iter().map(|r| self.ldb_reply(r)).collect())
            }
            other => other,
        }
    }
}

/// The words a debugger line is sent as: `e <code>` and `eval <code>` keep
/// the code whole; `None` for anything else, which splits as usual.
pub(crate) fn split_eval(line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let verb = [&b"eval "[..], b"e "].into_iter().find(|p| line.starts_with(p))?;
    Some(vec![line[..verb.len() - 1].to_vec(), line[verb.len()..].to_vec()])
}

/// A status line in its colour, when the terminal takes colour (TERM names
/// an xterm).
fn colored(line: &[u8]) -> Vec<u8> {
    if !std::env::var("TERM").is_ok_and(|t| t.contains("xterm")) {
        return line.to_vec();
    }
    let has = |tag: &[u8]| line.windows(tag.len()).any(|w| w == tag);
    let mut color = "white";
    for (tag, c) in [
        (&b"<debug>"[..], "bold"),
        (b"<command>", "green"),
        (b"<redis>", "green"),
        (b"<reply>", "cyan"),
        (b"<error>", "red"),
        (b"<hint>", "bold"),
        (b"<value>", "magenta"),
        (b"<retval>", "magenta"),
    ] {
        if has(tag) {
            color = c;
        }
    }
    if line.len() > 4 && line[3].is_ascii_digit() {
        if line[1] == b'>' {
            color = "yellow"; // the current line
        } else if line[2] == b'#' {
            color = "bold"; // a breakpoint
        }
    }
    let (bold, code) = match color {
        "bold" => (1, 37),
        "green" => (0, 32),
        "cyan" => (0, 36),
        "red" => (0, 31),
        "magenta" => (0, 35),
        "yellow" => (0, 33),
        _ => (0, 37),
    };
    [format!("\x1b[{bold};{code};49m").as_bytes(), line, b"\x1b[0m"].concat()
}

#[cfg(test)]
mod tests {
    use super::split_eval;

    #[test]
    fn eval_lines_keep_their_code_whole() {
        let words = |l: &str| {
            split_eval(l.as_bytes()).map(|v| {
                v.iter().map(|w| String::from_utf8_lossy(w).into_owned()).collect::<Vec<_>>()
            })
        };
        assert_eq!(
            words("e redis.call('GET', 'k')"),
            Some(vec!["e".into(), "redis.call('GET', 'k')".into()])
        );
        assert_eq!(words("eval  1+1"), Some(vec!["eval".into(), " 1+1".into()]));
        assert_eq!(words("print a"), None);
        assert_eq!(words("eval"), None);
    }
}
