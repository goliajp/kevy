//! Connections to cluster nodes: plain TCP, authenticated when `-a` says so,
//! and nothing else (no SELECT, no HELLO) — a node is spoken to in RESP2.

use super::addr::Addr;
use crate::rcli::conn::Conn;
use crate::rcli::opts::Opts;
use crate::rcli::session::eprint_bytes;
use kevy_resp::Reply;

/// Connect to `addr`, or print why not (`Could not connect to Redis at …`).
pub(crate) fn open(opts: &Opts, addr: &Addr) -> Option<Conn> {
    let opened = if addr.port < 0 {
        // A negative port is a service name the resolver does not know.
        Err("Servname not supported for ai_socktype".to_string())
    } else {
        Conn::tcp(&addr.host, addr.port, opts.connect_timeout)
    };
    let mut conn = match opened {
        Ok(conn) => conn,
        Err(why) => {
            let at = addr.shown();
            eprint_bytes(&[b"Could not connect to Redis at ", &at, b": ", why.as_bytes(), b"\n"]);
            return None;
        }
    };
    authenticate(opts, &mut conn).then_some(conn)
}

fn authenticate(opts: &Opts, conn: &mut Conn) -> bool {
    let Some(pass) = &opts.auth else { return true };
    let reply = match &opts.user {
        Some(user) => conn.request(&[b"AUTH", user, pass]),
        None => conn.request(&[b"AUTH", pass]),
    };
    match reply {
        Ok(Reply::Error(msg)) => {
            eprint_bytes(&[b"AUTH failed: ", &msg, b"\n"]);
            false
        }
        Ok(_) => true,
        Err(e) => {
            eprint_bytes(&[b"AUTH failed: ", e.text().as_bytes(), b"\n"]);
            false
        }
    }
}

/// A reply's text: a bulk, simple or verbatim string.
pub(crate) fn text(reply: &Reply) -> Option<&[u8]> {
    match reply {
        Reply::Bulk(b) | Reply::Simple(b) | Reply::Verbatim { data: b, .. } => Some(b),
        _ => None,
    }
}
