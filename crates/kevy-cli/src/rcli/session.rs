//! The client's session: the connection and what redis-cli tracks about it
//! (connect, AUTH, SELECT, HELLO 3, CLIENT SETNAME, in that order).

use super::conn::{Conn, LinkError};
use super::opts::Opts;
use kevy_resp::Reply;
use std::io::Write;

/// What happens to a RESP3 push that arrives while a reply is awaited —
/// the client's push handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PushSink {
    /// Print it in the current output mode.
    Print,
    /// Drop it unseen (the default).
    Discard,
    /// No callback: return it as the reply (set while (un)subscribing).
    Return,
}

/// Whether a failed connect says why.
///
/// Every connect here opens a new connection, dropping any old one: redis-cli
/// also has a connect-only-if-needed form, used by modes that later phases
/// implement, and it arrives with them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connect {
    /// Print `Could not connect to Redis at …` on failure.
    Report,
    /// Stay silent on failure.
    Quiet,
}

/// Connection plus state, redis-cli's `config` for the parts P0 needs.
pub(crate) struct Session {
    pub(crate) opts: Opts,
    pub(crate) conn: Option<Conn>,
    /// The last I/O failure, kept to report as `Error: …`.
    pub(crate) link_error: Option<LinkError>,
    pub(crate) dbnum: i32,
    pub(crate) in_multi: bool,
    pub(crate) pre_multi_dbnum: i32,
    pub(crate) pubsub_mode: bool,
    pub(crate) monitor_mode: bool,
    pub(crate) shutdown: bool,
    pub(crate) current_resp3: bool,
    pub(crate) push: PushSink,
    /// Standard input is being read as REPL lines (redis-cli's `interactive`).
    pub(crate) interactive: bool,
    /// The command reference, once something has needed it.
    pub(crate) docs: Option<std::rc::Rc<super::docs::model::Docs>>,
}

/// Whether the REPL shows argument hints (`:set hints` / `:set nohints`).
static HINTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub(crate) fn set_hints(on: bool) {
    HINTS.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn hints_on() -> bool {
    HINTS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Write to stderr; there is nowhere to report a failed diagnostic.
pub(crate) fn eprint_bytes(parts: &[&[u8]]) {
    let _ = std::io::stderr().write_all(&parts.concat()); // diagnostics only
}

impl Session {
    pub(crate) fn new(opts: Opts) -> Session {
        Session {
            opts,
            conn: None,
            link_error: None,
            dbnum: 0,
            in_multi: false,
            pre_multi_dbnum: 0,
            pubsub_mode: false,
            monitor_mode: false,
            shutdown: false,
            current_resp3: false,
            push: PushSink::Discard,
            interactive: false,
            docs: None,
        }
    }

    /// Open a connection and run the handshake. `true` when usable afterwards.
    pub(crate) fn connect(&mut self, how: Connect) -> bool {
        if self.conn.take().is_some() {
            self.dbnum = 0;
            self.in_multi = false;
            self.pubsub_mode = false;
        }
        let opened = match &self.opts.socket {
            Some(path) => Conn::unix(path),
            None => Conn::tcp(&self.opts.host, self.opts.port, self.opts.connect_timeout),
        };
        let conn = match opened {
            Ok(c) => c,
            Err(errstr) => {
                if how != Connect::Quiet {
                    self.print_connect_failure(&errstr);
                }
                return false;
            }
        };
        self.conn = Some(conn);
        self.link_error = None;
        self.current_resp3 = false;
        self.push = PushSink::Discard;
        if !(self.auth() && self.select() && self.switch_proto() && self.set_name()) {
            return false;
        }
        self.arm_push();
        true
    }

    fn arm_push(&mut self) {
        if self.opts.push_output {
            self.push = PushSink::Print;
        }
    }

    fn print_connect_failure(&self, errstr: &str) {
        let target = match &self.opts.socket {
            Some(path) => path.clone(),
            None => [self.opts.host.as_slice(), format!(":{}", self.opts.port).as_bytes()].concat(),
        };
        eprint_bytes(&[
            b"Could not connect to Redis at ",
            &target,
            b": ",
            errstr.as_bytes(),
            b"\n",
        ]);
    }

    /// One command during the handshake: its reply, or `None` on I/O error
    /// (reported as redis-cli reports a lost reply).
    fn handshake(&mut self, argv: &[&[u8]]) -> Option<Reply> {
        let conn = self.conn.as_mut()?;
        let argv: Vec<Vec<u8>> = argv.iter().map(|a| a.to_vec()).collect();
        match conn.send(&argv).and_then(|()| conn.read_reply()) {
            Ok((reply, _)) => Some(reply),
            Err(e) => {
                eprint_bytes(&[b"\nI/O error\n"]);
                self.link_error = Some(e);
                None
            }
        }
    }

    fn auth(&mut self) -> bool {
        let Some(pass) = self.opts.auth.clone() else { return true };
        let reply = match self.opts.user.clone() {
            None => self.handshake(&[b"AUTH", &pass]),
            Some(user) => self.handshake(&[b"AUTH", &user, &pass]),
        };
        expect_ok(reply, |msg| eprint_bytes(&[b"AUTH failed: ", msg, b"\n"]))
    }

    /// SELECT, only when the wanted db differs from the current one.
    pub(crate) fn select(&mut self) -> bool {
        if self.opts.input_dbnum == self.dbnum {
            return true;
        }
        let db = self.opts.input_dbnum.to_string();
        let reply = self.handshake(&[b"SELECT", db.as_bytes()]);
        let ok = expect_ok(reply, |msg| {
            eprint_bytes(&[b"SELECT ", db.as_bytes(), b" failed: ", msg, b"\n"])
        });
        if ok {
            self.dbnum = self.opts.input_dbnum;
        }
        ok
    }

    /// `HELLO 3` for `-3` (failure is fatal) or `--json` (failure tolerated).
    fn switch_proto(&mut self) -> bool {
        if self.opts.resp3 == 0 || self.opts.resp2 {
            return true;
        }
        let Some(reply) = self.handshake(&[b"HELLO", b"3"]) else { return false };
        let mut ok = true;
        if let Reply::Error(msg) | Reply::BlobError(msg) = &reply {
            eprint_bytes(&[b"HELLO 3 failed: ", super::format::c_str(msg), b"\n"]);
            ok = self.opts.resp3 != 1;
        }
        self.current_resp3 = true;
        ok
    }

    fn set_name(&mut self) -> bool {
        let Some(name) = self.opts.client_name.clone() else { return true };
        let reply = self.handshake(&[b"CLIENT", b"SETNAME", &name]);
        expect_ok(reply, |msg| eprint_bytes(&[b"CLIENT SETNAME failed: ", msg, b"\n"]))
    }

    /// Report the last I/O failure as `Error: …`; nothing without a connection.
    pub(crate) fn print_context_error(&self) {
        if let (Some(_), Some(e)) = (&self.conn, &self.link_error) {
            eprint_bytes(&[b"Error: ", e.text().as_bytes(), b"\n"]);
        }
    }
}

/// `true` unless the reply is missing or an error, which `report` prints.
fn expect_ok(reply: Option<Reply>, report: impl FnOnce(&[u8])) -> bool {
    match reply {
        None => false,
        Some(Reply::Error(msg) | Reply::BlobError(msg)) => {
            report(super::format::c_str(&msg));
            false
        }
        Some(_) => true,
    }
}
