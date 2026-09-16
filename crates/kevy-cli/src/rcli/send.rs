//! Sending a command and reading what comes back, with the client-side
//! bookkeeping redis-cli keeps around commands.

use super::format::{Output, invalidate_tty, is_invalidate, is_verbatim_command, render};
use super::session::{Connect, PushSink, Session, eprint_bytes};
use kevy_resp::Reply;
use std::io::Write;

/// The outcome of reading one reply.
pub(crate) enum Read {
    /// A reply was read (and printed unless suppressed).
    Reply(Reply),
    /// The connection is gone in a way the caller handles.
    Failed,
    /// Ctrl-C cut a subscribed or monitoring connection; a fresh one replaced it.
    Interrupted,
}

fn is(argv: &[Vec<u8>], i: usize, word: &str) -> bool {
    argv.get(i).is_some_and(|a| a.eq_ignore_ascii_case(word.as_bytes()))
}

pub(crate) fn write_out(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(bytes).and_then(|()| out.flush()); // a closed stdout has nobody to tell
}

impl Session {
    /// Run a command `repeat` times: `help` stays local, a lost link reconnects.
    pub(crate) fn issue(&mut self, argv: &[Vec<u8>], repeat: i64) -> bool {
        if is(argv, 0, "help") || is(argv, 0, "?") {
            self.print_help(&argv[1..]);
            return true;
        }
        if self.conn.is_none() || self.link_error.is_some() {
            if !self.connect(Connect::Report) {
                self.print_context_error();
                return false;
            }
            self.dbnum = 0;
            self.select();
        }
        if !self.send_command(argv, repeat) {
            self.print_context_error();
            self.conn = None;
            self.link_error = None;
            return false;
        }
        true
    }

    /// Send, read, and track what the command changes client-side.
    fn send_command(&mut self, argv: &[Vec<u8>], mut repeat: i64) -> bool {
        let verbatim = is_verbatim_command(argv);
        if is(argv, 0, "shutdown") {
            self.shutdown = true;
        }
        if is(argv, 0, "monitor") {
            self.monitor_mode = true;
        }
        let subscribe =
            is(argv, 0, "subscribe") || is(argv, 0, "psubscribe") || is(argv, 0, "ssubscribe");
        let unsubscribe = is(argv, 0, "unsubscribe")
            || is(argv, 0, "punsubscribe")
            || is(argv, 0, "sunsubscribe");
        // Negative repeats forever, as redis-cli's `-r -1` does.
        while repeat != 0 {
            repeat -= 1;
            let Some(conn) = self.conn.as_mut() else { return false };
            if let Err(e) = conn.send(argv) {
                self.link_error = Some(e);
                return false;
            }
            if self.monitor_mode {
                return self.monitor_loop(verbatim);
            }
            let expected = if subscribe || unsubscribe {
                self.push = PushSink::Return;
                argv.len().saturating_sub(1).max(1)
            } else {
                0
            };
            if !self.await_reply(argv, verbatim, expected, subscribe, unsubscribe) {
                return false;
            }
            if self.opts.interval_us > 0 {
                std::thread::sleep(std::time::Duration::from_micros(self.opts.interval_us));
            }
        }
        true
    }

    fn monitor_loop(&mut self, verbatim: bool) -> bool {
        loop {
            match self.read_reply(verbatim) {
                Read::Reply(Reply::Error(_) | Reply::BlobError(_)) => {
                    self.monitor_mode = false;
                    return true;
                }
                Read::Reply(_) => {}
                Read::Interrupted => return true,
                Read::Failed => {
                    self.print_context_error();
                    std::process::exit(1);
                }
            }
        }
    }

    /// Read until this command's own reply: skip pub/sub traffic until the
    /// reply to this command, then update the tracked state.
    fn await_reply(
        &mut self,
        argv: &[Vec<u8>],
        verbatim: bool,
        mut expected: usize,
        subscribe: bool,
        unsubscribe: bool,
    ) -> bool {
        loop {
            let reply = match self.read_reply(verbatim) {
                Read::Reply(r) => r,
                Read::Interrupted => return true,
                Read::Failed => return false,
            };
            if self.pubsub_mode || expected > 0 {
                if let Some(kind) = pubsub_kind(&reply, self.current_resp3) {
                    if expected > 0 && kind.eq_ignore_ascii_case(&argv[0]) {
                        if subscribe && !self.pubsub_mode {
                            self.pubsub_mode = true;
                        }
                        expected -= 1;
                        if expected > 0 {
                            continue;
                        }
                    } else {
                        continue;
                    }
                } else if matches!(reply, Reply::Push(_)) {
                    continue;
                }
            }
            self.track_state(argv, &reply, subscribe || unsubscribe);
            return true;
        }
    }

    // LOC-WAIVER: a dispatch table over the commands whose success changes
    // client-side state, one arm each, in redis-cli's order.
    fn track_state(&mut self, argv: &[Vec<u8>], reply: &Reply, pubsub_cmd: bool) {
        let error = matches!(reply, Reply::Error(_) | Reply::BlobError(_));
        let n = argv.len();
        if is(argv, 0, "select") && n == 2 && !error {
            let db = super::cnum::atoi(&argv[1]);
            self.opts.input_dbnum = db;
            self.dbnum = db;
        } else if is(argv, 0, "auth") && (n == 2 || n == 3) {
            self.select();
        } else if is(argv, 0, "multi") && n == 1 && !error {
            self.in_multi = true;
            self.pre_multi_dbnum = self.dbnum;
        } else if is(argv, 0, "exec") && n == 1 && self.in_multi {
            self.in_multi = false;
            if error || matches!(reply, Reply::Nil | Reply::Null) {
                self.opts.input_dbnum = self.pre_multi_dbnum;
                self.dbnum = self.pre_multi_dbnum;
            }
        } else if is(argv, 0, "discard") && n == 1 && !error {
            self.in_multi = false;
            self.opts.input_dbnum = self.pre_multi_dbnum;
            self.dbnum = self.pre_multi_dbnum;
        } else if is(argv, 0, "reset") && n == 1 && !error {
            self.in_multi = false;
            self.dbnum = 0;
            self.opts.input_dbnum = 0;
            self.current_resp3 = false;
            if self.pubsub_mode && self.opts.push_output {
                self.push = PushSink::Print;
            }
            self.pubsub_mode = false;
        } else if is(argv, 0, "hello") {
            match reply {
                Reply::Map(_) => self.current_resp3 = true,
                Reply::Array(_) => self.current_resp3 = false,
                _ => {}
            }
        } else if pubsub_cmd && !self.pubsub_mode && self.opts.push_output {
            self.push = PushSink::Print;
        }
    }

    /// One reply off the wire (pushes routed to the sink),
    /// printed in the current output mode.
    pub(crate) fn read_reply(&mut self, verbatim: bool) -> Read {
        self.arm_interrupt();
        let (reply, texts) = loop {
            let Some(conn) = self.conn.as_mut() else { return Read::Failed };
            match conn.read_reply() {
                Ok((reply @ Reply::Push(_), texts)) if self.push != PushSink::Return => {
                    if self.push == PushSink::Print {
                        self.print_push(&reply, &texts);
                    }
                }
                Ok(parsed) => break parsed,
                Err(e) => return self.read_failed(e),
            }
        };
        if !self.interactive
            && self.opts.set_errcode
            && let Reply::Error(msg) | Reply::BlobError(msg) = &reply
        {
            eprint_bytes(&[super::format::c_str(msg), b"\n"]);
            std::process::exit(1);
        }
        write_out(&render(&reply, &texts, self.opts.output, &self.opts.delims, verbatim));
        Read::Reply(reply)
    }

    fn read_failed(&mut self, e: super::conn::LinkError) -> Read {
        if kevy_sys::take_severed() {
            self.recover_from_interrupt();
            return Read::Interrupted;
        }
        if self.shutdown {
            self.conn = None;
            return Read::Reply(Reply::Nil);
        }
        let survivable = self.interactive && e.is_reconnectable();
        self.link_error = Some(e);
        if survivable {
            return Read::Failed;
        }
        self.print_context_error();
        std::process::exit(1);
    }

    /// Ctrl-C while subscribed or monitoring: leave the mode on a new
    /// connection, or report and exit when there is none to be had.
    pub(crate) fn recover_from_interrupt(&mut self) {
        self.monitor_mode = false;
        self.pubsub_mode = false;
        if !self.connect(Connect::Report) {
            self.print_context_error();
            std::process::exit(1);
        }
        self.arm_interrupt();
    }

    /// Point Ctrl-C at the connection when it is streaming, else at exiting.
    pub(crate) fn arm_interrupt(&self) {
        let streaming = self.interactive && (self.monitor_mode || self.pubsub_mode);
        kevy_sys::sever_on_interrupt(
            self.conn.as_ref().filter(|_| streaming).map(super::conn::Conn::fd),
        );
    }

    /// Print a push that arrived while a reply was awaited.
    pub(crate) fn print_push(&self, reply: &Reply, texts: &[Vec<u8>]) {
        write_out(&self.push_bytes(reply, texts));
    }

    /// What is printed for a push that arrived while a reply was awaited.
    pub(crate) fn push_bytes(&self, reply: &Reply, texts: &[Vec<u8>]) -> Vec<u8> {
        if self.opts.output == Output::Standard && is_invalidate(reply) {
            invalidate_tty(reply)
        } else {
            render(reply, texts, self.opts.output, &self.opts.delims, false)
        }
    }
}

/// The kind (`message`, `subscribe`, …) of a pub/sub frame, if it is one.
fn pubsub_kind(r: &Reply, resp3: bool) -> Option<&[u8]> {
    let items = match (r, resp3) {
        (Reply::Push(items), true) | (Reply::Array(items), false) => items,
        _ => return None,
    };
    let Some(Reply::Bulk(kind)) = items.first() else { return None };
    (items.len() >= 3 && (kind.ends_with(b"message") || kind.ends_with(b"subscribe")))
        .then_some(kind.as_slice())
}
