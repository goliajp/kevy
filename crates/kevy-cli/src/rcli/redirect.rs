//! `-c`: follow a cluster's MOVED and ASK redirections to the node that owns
//! the key, and stay connected there.

use super::send::write_out;
use super::session::{Connect, Session};
use kevy_resp::Reply;

/// Where an error reply sends the command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// ASK: the command goes there once, after ASKING.
    pub(crate) ask: bool,
    pub(crate) slot: u16,
    /// Empty when the owner's endpoint is unknown: the current host.
    pub(crate) host: Vec<u8>,
    pub(crate) port: i32,
}

/// `MOVED <slot> <host>:<port>` or `ASK <slot> <host>:<port>`; the port is
/// after the last colon, so an IPv6 host keeps its own.
pub(crate) fn parse(reply: &Reply) -> Option<Redirect> {
    let Reply::Error(msg) = reply else { return None };
    let (ask, rest) = if let Some(rest) = msg.strip_prefix(b"MOVED ") {
        (false, rest)
    } else {
        (true, msg.strip_prefix(b"ASK ")?)
    };
    let space = rest.iter().position(|&b| b == b' ')?;
    let slot = std::str::from_utf8(&rest[..space]).ok()?.parse().ok()?;
    let endpoint = &rest[space + 1..];
    let colon = endpoint.iter().rposition(|&b| b == b':')?;
    let port = std::str::from_utf8(&endpoint[colon + 1..]).ok()?.parse().ok()?;
    Some(Redirect { ask, slot, host: endpoint[..colon].to_vec(), port })
}

impl Session {
    /// Connect to the node a redirection names; for ASK, say ASKING there.
    /// `false` when that node cannot be reached.
    pub(crate) fn follow(&mut self, to: Redirect) -> bool {
        let host =
            if to.host.is_empty() || to.host == b"?" { self.opts.host.clone() } else { to.host };
        if self.interactive {
            let line = format!("-> Redirected to slot [{}] located at ", to.slot);
            write_out(&[line.as_bytes(), &host, format!(":{}\n", to.port).as_bytes()].concat());
        }
        self.opts.host = host;
        self.opts.port = to.port;
        if !self.connect(Connect::Report) {
            return false;
        }
        if !to.ask {
            return true;
        }
        let Some(conn) = self.conn.as_mut() else { return false };
        let asked = conn.send(&[b"ASKING".to_vec()]).and_then(|()| conn.read_reply());
        match asked {
            Ok(_) => true,
            Err(e) => {
                self.link_error = Some(e);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Redirect, parse};
    use kevy_resp::Reply;

    fn err(s: &str) -> Reply {
        Reply::Error(s.as_bytes().to_vec())
    }

    #[test]
    fn redirections_name_a_slot_and_an_endpoint() {
        let to = |ask, slot, host: &str, port| Redirect { ask, slot, host: host.into(), port };
        assert_eq!(
            parse(&err("MOVED 12182 127.0.0.1:7002")),
            Some(to(false, 12182, "127.0.0.1", 7002))
        );
        assert_eq!(parse(&err("ASK 3 ::1:7000")), Some(to(true, 3, "::1", 7000)));
        assert_eq!(parse(&err("MOVED 1 :7000")), Some(to(false, 1, "", 7000)));
        assert_eq!(parse(&err("MOVED x 1:2")), None);
        assert_eq!(parse(&err("ERR MOVED 1 a:2")), None);
        assert_eq!(parse(&Reply::Simple(b"MOVED 1 a:2".to_vec())), None);
    }
}
