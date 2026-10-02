//! `CLIENT SETNAME` / `CLIENT GETNAME` interception.
//!
//! These two subcommands need per-connection state which the
//! stateless `cmd_client` dispatch in `kevy` can't access. We
//! intercept them at the reactor level — `handle_command` already
//! owns `&mut Conn` via `self.conns.get_mut(conn_id)` — and emit
//! the reply directly with `immediate_reply`.
//!
//! All other CLIENT subcommands (`ID`, `LIST`, `INFO`, `KILL`,
//! `NO-EVICT`, etc.) fall through to the standard dispatch path.
//!
//! Lives outside `exec.rs` to keep that file under the 500-LOC
//! house rule.

use kevy_resp::ArgvView;

use crate::Commands;
use crate::shard::Shard;

impl<C: Commands> Shard<C> {
    /// Return `true` when `args` is `CLIENT SETNAME <name>` or
    /// `CLIENT GETNAME` and the intercept emitted a reply.
    pub(crate) fn try_intercept_client<A: ArgvView + ?Sized>(
        &mut self,
        conn_id: u64,
        args: &A,
    ) -> bool {
        if args.len() < 2 {
            return false;
        }
        let Some(verb) = args.get(0) else { return false };
        if !verb.eq_ignore_ascii_case(b"CLIENT") {
            return false;
        }
        let Some(sub) = args.get(1) else { return false };
        let sub_upper = sub.to_ascii_uppercase();
        match sub_upper.as_slice() {
            b"SETNAME" => {
                self.client_setname(conn_id, args);
                true
            }
            b"GETNAME" => {
                self.client_getname(conn_id, args);
                true
            }
            b"ID" if args.len() == 2 => {
                // Conn ids stride by shard count from a per-shard
                // start — unique across the instance, so the value is
                // a valid CLIENT KILL ID target.
                self.immediate_reply(conn_id, format!(":{conn_id}\r\n").into_bytes());
                true
            }
            b"INFO" if args.len() == 2 => {
                self.client_info(conn_id);
                true
            }
            b"SETPEER" if self.peer_token.is_some() => {
                self.client_setpeer(conn_id, args);
                true
            }
            _ => false,
        }
    }

    /// `CLIENT INFO` arm — this connection's own row, rendered by the
    /// same renderer CLIENT LIST uses (bulk under RESP2, verbatim
    /// `txt` under RESP3). Split out per the 50-LOC fn rule.
    #[inline(always)]
    fn client_info(&mut self, conn_id: u64) {
        let Some(conn) = self.conns.get(&conn_id) else { return };
        let mut row = Vec::with_capacity(224);
        crate::client_ops::client_row(conn_id, conn, &mut row);
        // The row renderer terminates lines for LIST concatenation;
        // INFO is a single row without the trailing newline.
        if row.last() == Some(&b'\n') {
            row.pop();
        }
        let mut out = Vec::with_capacity(row.len() + 24);
        match conn.proto {
            kevy_resp::RespVersion::V2 => kevy_resp::encode_bulk(&mut out, &row),
            kevy_resp::RespVersion::V3 => kevy_resp::encode_verbatim(&mut out, *b"txt", &row),
        }
        self.immediate_reply(conn_id, out);
    }

    /// `CLIENT SETPEER <token> <ip:port>` — the front end relaying this
    /// connection names the client it relays for.
    fn client_setpeer<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) {
        let reply: &[u8] = match (args.len(), self.peer_token, args.get(2), args.get(3)) {
            (4, Some(want), Some(token), Some(addr)) => {
                let token_ok = token_matches(token, &want);
                let addr = std::str::from_utf8(addr)
                    .ok()
                    .and_then(|a| a.parse::<std::net::SocketAddrV4>().ok());
                match (token_ok, addr, self.conns.get_mut(&conn_id)) {
                    (true, Some(a), Some(c)) => {
                        c.peer = (*a.ip(), a.port());
                        c.relayed = true;
                        b"+OK\r\n"
                    }
                    (false, ..) => b"-ERR invalid peer token\r\n",
                    _ => b"-ERR invalid peer address\r\n",
                }
            }
            _ => b"-ERR wrong number of arguments for 'client|setpeer'\r\n",
        };
        self.immediate_reply(conn_id, reply.to_vec());
    }

    /// `CLIENT SETNAME <name>` arm — extracted verbatim from
    /// [`Self::try_intercept_client`] (single call site, `inline(always)`)
    /// purely for the 50-LOC fn rule.
    #[inline(always)]
    fn client_setname<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) {
        if args.len() != 3 {
            self.immediate_reply(
                conn_id,
                b"-ERR wrong number of arguments for 'client|setname'\r\n".to_vec(),
            );
            return;
        }
        let name = args.get(2).unwrap_or(&[]);
        // Redis disallows whitespace + control bytes in the
        // name (the LIST output would be ambiguous otherwise).
        if name.iter().any(|b| b.is_ascii_whitespace() || *b < 0x20) {
            self.immediate_reply(
                conn_id,
                b"-ERR Client names cannot contain spaces, newlines or special characters.\r\n"
                    .to_vec(),
            );
            return;
        }
        if let Some(c) = self.conns.get_mut(&conn_id) {
            c.client_name.clear();
            c.client_name.extend_from_slice(name);
        }
        self.immediate_reply(conn_id, b"+OK\r\n".to_vec());
    }

    /// `CLIENT GETNAME` arm — extracted verbatim from
    /// [`Self::try_intercept_client`] (single call site, `inline(always)`)
    /// purely for the 50-LOC fn rule.
    #[inline(always)]
    fn client_getname<A: ArgvView + ?Sized>(&mut self, conn_id: u64, args: &A) {
        if args.len() != 2 {
            self.immediate_reply(
                conn_id,
                b"-ERR wrong number of arguments for 'client|getname'\r\n".to_vec(),
            );
            return;
        }
        let Some(c) = self.conns.get(&conn_id) else { return };
        // an empty name is no name, which reads back as null
        let out = match (c.client_name.is_empty(), c.proto) {
            (true, kevy_resp::RespVersion::V3) => b"_\r\n".to_vec(),
            (true, _) => b"$-1\r\n".to_vec(),
            (false, _) => {
                let mut out = Vec::new();
                kevy_resp::encode_bulk(&mut out, &c.client_name);
                out
            }
        };
        self.immediate_reply(conn_id, out);
    }
}

/// Decode all 64 hex characters first, then compare in constant time, so
/// the reply's timing says nothing about how much of the token was right.
fn token_matches(hex: &[u8], want: &[u8; 32]) -> bool {
    let mut got = [0u8; 32];
    if hex.len() != 64 {
        return false;
    }
    for (i, b) in got.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&hex[2 * i..2 * i + 2]).ok();
        match pair.and_then(|h| u8::from_str_radix(h, 16).ok()) {
            Some(v) => *b = v,
            None => return false,
        }
    }
    kevy_crypto::ct_eq(&got, want)
}

#[cfg(test)]
mod tests {
    use super::token_matches;

    #[test]
    fn a_peer_token_matches_only_its_own_64_hex_characters() {
        let want = [0xab; 32];
        let hex = "ab".repeat(32);
        assert!(token_matches(hex.as_bytes(), &want));
        assert!(token_matches(hex.to_uppercase().as_bytes(), &want));
        let mut last_off = hex.clone().into_bytes();
        last_off[63] = b'c';
        assert!(!token_matches(&last_off, &want));
        assert!(!token_matches(&hex.as_bytes()[..62], &want));
        assert!(!token_matches("zz".repeat(32).as_bytes(), &want));
        assert!(!token_matches("é".repeat(32).as_bytes(), &want));
    }
}
