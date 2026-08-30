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
        if let Some(c) = self.conns.get(&conn_id) {
            let name = c.client_name.clone();
            let mut out = Vec::with_capacity(8 + name.len());
            out.extend_from_slice(format!("${}\r\n", name.len()).as_bytes());
            out.extend_from_slice(&name);
            out.extend_from_slice(b"\r\n");
            self.immediate_reply(conn_id, out);
        } else {
            self.immediate_reply(conn_id, b"$0\r\n\r\n".to_vec());
        }
    }
}
