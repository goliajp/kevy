//! The REPL prompt: where the session is connected and what state it is in.

use super::session::Session;

/// Longest prompt, in bytes.
const MAX_PROMPT: usize = 127;

/// `host:port[db](TX)(subscribed mode)> `, `kevy <socket>> …`, or
/// `not connected> `.
pub(crate) fn prompt(s: &Session) -> Vec<u8> {
    if s.conn.is_none() {
        return b"not connected> ".to_vec();
    }
    let mut p = match &s.opts.socket {
        // DEV-011: redis-cli names itself here ("redis <path>").
        Some(path) => [b"kevy ".as_slice(), path].concat(),
        None if s.opts.host.contains(&b':') => [b"[".as_slice(), &s.opts.host, b"]"].concat(),
        None => s.opts.host.clone(),
    };
    if s.opts.socket.is_none() {
        p.extend_from_slice(format!(":{}", s.opts.port).as_bytes());
    }
    if s.dbnum != 0 {
        p.extend_from_slice(format!("[{}]", s.dbnum).as_bytes());
    }
    if s.in_multi {
        p.extend_from_slice(b"(TX)");
    }
    if s.pubsub_mode {
        p.extend_from_slice(b"(subscribed mode)");
    }
    p.truncate(MAX_PROMPT - 2);
    p.extend_from_slice(b"> ");
    p
}
